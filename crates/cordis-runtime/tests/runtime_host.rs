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
use serial_test::serial;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

mod support;

use support::{
    fixtures_root, pin_private_snapshot_root, spawn_chunked_mock_llm_server_sequence, sse_response,
};

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
    // fixtures root 就是 temp.path() 本身，`discover_config_dir` 落到
    // `temp/config`（同级 `{TMPDIR}/config` 不存在，目录名也不是 "fixtures"）。
    pin_private_snapshot_root(temp.path(), temp.path());
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
    // fixtures root 目录名是 "fixtures"，`discover_config_dir` 走同级分支 →
    // `temp/config`（不在 fixtures 目录内部）。
    pin_private_snapshot_root(temp.path(), &temp_fixtures);
    temp
}

fn write_llm_api_config(root: &Path, base_url: &str, timeout_ms: u64) {
    // provider 插件在请求时从环境读 key（契约类型不带明文 key）。
    std::env::set_var("CORDIS_TEST_LLM_KEY", "test-key");
    let config_dir = root.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: deepseek\nbase_url: {base_url}\napi_key_env: CORDIS_TEST_LLM_KEY\nmodel: deepseek-reasoner\ntemperature: 0.0\nmax_tokens: 4096\ntimeout_ms: {timeout_ms}\n"
        ),
    )
    .expect("write llm api config");
}

fn read_rel(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).expect("read fixture file")
}

fn replace_once(content: &str, old: &str, new: &str) -> String {
    let replaced = content.replacen(old, new, 1);
    assert_ne!(
        replaced, content,
        "replacement should change fixture content"
    );
    replaced
}

fn tool_call_response(
    response_id: &str,
    tool_calls: Vec<(&str, &str, Value)>,
) -> Vec<(u64, String)> {
    sse_response(vec![
        json!({
            "id": response_id,
            "choices": [{
                "delta": {
                    "tool_calls": tool_calls
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
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        }),
    ])
}

fn assistant_response(response_id: &str, content: &str) -> Vec<(u64, String)> {
    sse_response(vec![
        json!({
            "id": response_id,
            "choices": [{
                "delta": {
                    "content": content,
                }
            }]
        }),
        json!({
            "id": response_id,
            "choices": [{
                "delta": {},
                "finish_reason": "stop"
            }]
        }),
    ])
}

// 5.2.27-1: these fixtures previously scripted the agent adding a `%`
// (modulo) operator — but the real expr fixture gained modulo in
// `2eb0ff4` (2026-06-02), so the scripted `replace_once` anchors began
// asserting "content unchanged" and the tests rotted. The scripted
// feature is now a `~` (absolute-difference "dist") operator, which the
// fixture does NOT have; same shape (new token + parser arm + evaluator
// child plugin), same promote/retry flows.
fn generated_dist_scaffold_core() -> &'static str {
    "use serde::{Deserialize, Serialize};\nuse thiserror::Error;\n\n#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]\n#[serde(rename_all = \"snake_case\")]\npub enum DistError {\n    #[error(\"not implemented\")]\n    NotImplemented,\n}\n\n#[derive(Debug, Default, Clone, Copy)]\npub struct DistPlugin;\n\nimpl DistPlugin {\n    pub fn apply(&self, _lhs: f64, _rhs: f64) -> Result<f64, DistError> {\n        Err(DistError::NotImplemented)\n    }\n}\n\n#[allow(dead_code)]\npub fn apply(lhs: f64, rhs: f64) -> Result<f64, DistError> {\n    DistPlugin.apply(lhs, rhs)\n}\n"
}

fn implemented_dist_core() -> &'static str {
    "use serde::{Deserialize, Serialize};\nuse thiserror::Error;\n\n#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]\n#[serde(rename_all = \"snake_case\")]\npub enum DistError {\n    #[error(\"not implemented\")]\n    NotImplemented,\n}\n\n#[derive(Debug, Default, Clone, Copy)]\npub struct DistPlugin;\n\nimpl DistPlugin {\n    pub fn apply(&self, lhs: f64, rhs: f64) -> Result<f64, DistError> {\n        Ok((lhs - rhs).abs())\n    }\n}\n\n#[allow(dead_code)]\npub fn apply(lhs: f64, rhs: f64) -> Result<f64, DistError> {\n    DistPlugin.apply(lhs, rhs)\n}\n"
}

fn implemented_dist_core_with_warning() -> &'static str {
    "use serde::{Deserialize, Serialize};\nuse std::fmt;\nuse thiserror::Error;\n\n#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]\n#[serde(rename_all = \"snake_case\")]\npub enum DistError {\n    #[error(\"not implemented\")]\n    NotImplemented,\n}\n\n#[derive(Debug, Default, Clone, Copy)]\npub struct DistPlugin;\n\nimpl DistPlugin {\n    pub fn apply(&self, lhs: f64, rhs: f64) -> Result<f64, DistError> {\n        Ok((lhs - rhs).abs())\n    }\n}\n\n#[allow(dead_code)]\npub fn apply(lhs: f64, rhs: f64) -> Result<f64, DistError> {\n    DistPlugin.apply(lhs, rhs)\n}\n"
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

fn plugin_iteration_journal_path(snapshot_root: &str) -> PathBuf {
    PathBuf::from(snapshot_root).join("plugin-iteration-edit-journal.json")
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    add_demo_process_plugin(temp.path(), "v1");
    let report = host.reload("/").expect("reload with demo should succeed");

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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    let report = host
        .reload("/")
        .expect("reload without shell should succeed");

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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let snapshot_id = host.current_snapshot().snapshot_id().to_string();

    overwrite_index_hash(temp.path(), "shell", "deadbeef");
    let err = host
        .reload("/")
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    // 5.2.27: semantics updated for the P0-14/P2-34 docs auto-heal. The
    // dylib's embedded docs are ground truth; tampering with the cached
    // copy in index.json no longer propagates a "docs_changed" snapshot
    // diff — instead the loader detects the drift and heals the cache
    // BACK from the artifact. Assert the heal, not the old propagation.
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let original_summary =
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry");
    let tampered_summary = "Start the CordisClaw terminal with updated docs.";

    update_index_node_summary(temp.path(), "shell", "shell_entry", tampered_summary);
    host.reload("/").expect("reload should succeed");

    // Auto-heal wins: the live snapshot serves the artifact's original
    // summary, not the tampered cache entry.
    assert_eq!(
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry"),
        original_summary
    );
    // And the on-disk cache is healed back too.
    let index_text =
        fs::read_to_string(temp.path().join("artifacts/index.json")).expect("read healed index");
    assert!(
        !index_text.contains(tampered_summary),
        "tampered summary must be healed out of index.json"
    );
}

#[test]
fn runtime_host_snapshot_keeps_old_staged_process_artifact_after_reload() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_copy();
    add_demo_process_plugin(temp.path(), "v1");
    let host = RuntimeHost::boot(temp.path()).expect("host should boot with demo");
    let old_snapshot = host.current_snapshot();
    let old_stage = old_snapshot.staged_artifact_root().to_path_buf();

    write_demo_artifacts(temp.path(), "v2");
    sync_demo_index_entry(temp.path(), "v2");
    refresh_artifact_index(temp.path()).expect("refresh index after demo update");
    host.reload("/")
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    host.reload("/").expect("reload should succeed");

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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let snapshot_id = host.current_snapshot().snapshot_id().to_string();

    overwrite_index_hash(temp.path(), "shell", "deadbeef");
    let report = host.reload_with_diagnostics("/");

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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let journal_path = plugin_iteration_journal_path(&host.status().snapshot_root);
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
    assert!(!journal_path.exists());
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

#[serial]
#[test]
fn runtime_host_iterate_plugins_agent_adds_dist_child_plugin_and_promotes() {
    // LLM-network coverage: drives the mock SSE server through a full
    // agent-driven iteration, exercising `PluginIterationAgentBackend::
    // execute_tool` arms scaffold_child_plugin / replace_files_exact /
    // run_plugin_test / record_iteration_summary. No longer linux-gated:
    // `ensure_fixture_artifacts` rebuilds fixture dylibs for the local target.
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");

    let lexer_before = read_rel(&fixtures, "plugins/expr/lexer/src/core.rs");
    let lexer_after = replace_once(
        &lexer_before,
        "    Exclamation,\n    LParen,\n",
        "    Exclamation,\n    Tilde,\n    LParen,\n",
    );
    let lexer_after = replace_once(
        &lexer_after,
        "            '!' => {\n                pos += 1;\n                Token {\n                    kind: TokenKind::Exclamation,\n                    position: pos - 1,\n                }\n            }\n            '(' => {\n",
        "            '!' => {\n                pos += 1;\n                Token {\n                    kind: TokenKind::Exclamation,\n                    position: pos - 1,\n                }\n            }\n            '~' => {\n                pos += 1;\n                Token {\n                    kind: TokenKind::Tilde,\n                    position: pos - 1,\n                }\n            }\n            '(' => {\n",
    );

    let parser_before = read_rel(&fixtures, "plugins/expr/parser/src/core.rs");
    let parser_after = replace_once(&parser_before, "    Pow,\n}\n", "    Pow,\n    Dist,\n}\n");
    let parser_after = replace_once(
        &parser_after,
        "                Some(TokenKind::Percent) => BinaryOp::Mod,\n                _ => break,\n",
        "                Some(TokenKind::Percent) => BinaryOp::Mod,\n                Some(TokenKind::Tilde) => BinaryOp::Dist,\n                _ => break,\n",
    );

    let evaluator_before = read_rel(&fixtures, "plugins/expr/evaluator/src/core.rs");
    let evaluator_after = replace_once(
        &evaluator_before,
        "#[path = \"../div/src/core.rs\"]\npub mod div_core;\n",
        "#[path = \"../div/src/core.rs\"]\npub mod div_core;\n#[path = \"../dist/src/core.rs\"]\npub mod dist_core;\n",
    );
    let evaluator_after = replace_once(
        &evaluator_after,
        "pub use div_core::{DivError, DivPlugin};\n",
        "pub use div_core::{DivError, DivPlugin};\npub use dist_core::{DistError, DistPlugin};\n",
    );
    let evaluator_after = replace_once(
        &evaluator_after,
        "    div: DivPlugin,\n",
        "    div: DivPlugin,\n    dist: DistPlugin,\n",
    );
    let evaluator_after = replace_once(
        &evaluator_after,
        "                BinaryOp::Div => ops.div.apply(left, right).map_err(|err| match err {\n                    DivError::DivisionByZero => EvalError::DivisionByZero,\n                }),\n",
        "                BinaryOp::Div => ops.div.apply(left, right).map_err(|err| match err {\n                    DivError::DivisionByZero => EvalError::DivisionByZero,\n                }),\n                BinaryOp::Dist => ops.dist.apply(left, right).map_err(|err| match err {\n                    DistError::NotImplemented => EvalError::NonFinite,\n                }),\n",
    );

    let eval_tests_before = read_rel(&fixtures, "plugins/expr/tests/eval.rs");
    let eval_tests_after = format!(
        "{eval_tests_before}\n#[test]\nfn evaluates_dist_expression() {{\n    let value = evaluate_expression(\"7 ~ 4 + 1\").expect(\"must evaluate\");\n    assert_eq!(value, 4.0);\n}}\n"
    );

    let responses = vec![
        tool_call_response(
            "chatcmpl_plugin_iter_1",
            vec![(
                "call_scaffold_mod",
                "scaffold_child_plugin",
                json!({
                    "parent_plugin_path": "expr/evaluator",
                    "child_name": "dist",
                    "node_id": "expr_dist",
                    "summary": "Compute absolute difference of lhs and rhs."
                }),
            )],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_2",
            vec![
                (
                    "call_replace_lexer",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/lexer/src/core.rs",
                        "expected_old_string": lexer_before,
                        "new_content": lexer_after,
                    }),
                ),
                (
                    "call_replace_parser",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/parser/src/core.rs",
                        "expected_old_string": parser_before,
                        "new_content": parser_after,
                    }),
                ),
                (
                    "call_replace_evaluator",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/evaluator/src/core.rs",
                        "expected_old_string": evaluator_before,
                        "new_content": evaluator_after,
                    }),
                ),
                (
                    "call_replace_mod_core",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/evaluator/dist/src/core.rs",
                        "expected_old_string": generated_dist_scaffold_core(),
                        "new_content": implemented_dist_core(),
                    }),
                ),
                (
                    "call_replace_eval_tests",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/tests/eval.rs",
                        "expected_old_string": eval_tests_before,
                        "new_content": eval_tests_after,
                    }),
                ),
            ],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_3",
            vec![(
                "call_run_tests",
                "run_plugin_test",
                json!({
                    "command": "cargo test --quiet --manifest-path plugins/expr/Cargo.toml"
                }),
            )],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_4",
            vec![(
                "call_record_summary",
                "record_iteration_summary",
                json!({
                    "summary": "Add dist child plugin support under expr/evaluator/dist and wire lexer/parser/evaluator dispatch.",
                    "tests_command": "cargo test --quiet --manifest-path plugins/expr/Cargo.toml"
                }),
            )],
        ),
    ];
    let (base_url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(responses);
    write_llm_api_config(temp.path(), &base_url, 120_000);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    let seed = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "1 + 2 * 3" }).to_string(),
        )
        .expect("expr invoke should seed canary replay");
    let seed_value: Value = serde_json::from_str(&seed.payload).expect("seed response json");
    assert_eq!(seed_value.get("value").and_then(|v| v.as_f64()), Some(7.0));

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["expr".to_string()],
            instruction: Some("Add absolute-difference (~) operator support with a sibling evaluator child plugin at expr/evaluator/dist.".to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect("agent-driven plugin iteration should succeed");

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");

    assert_eq!(
        requests.len(),
        4,
        "record_iteration_summary should end the session"
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
    assert_eq!(
        result.canary.as_ref().map(|report| report.verdict),
        Some(CanaryVerdict::Pass)
    );
    assert!(result
        .changed_paths
        .iter()
        .any(|path| path == "plugins/expr/evaluator/dist/Cargo.toml"));
    assert!(result
        .changed_paths
        .iter()
        .any(|path| path == "plugins/expr/lexer/src/core.rs"));
    assert!(result
        .changed_paths
        .iter()
        .all(|path| !path.contains("/modulo/")));
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("expr/evaluator/dist")
        .is_some());

    let dist_response = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "7 ~ 4 + 1" }).to_string(),
        )
        .expect("promoted expr plugin should support dist");
    let dist_value: Value =
        serde_json::from_str(&dist_response.payload).expect("dist response json");
    assert_eq!(dist_value.get("value").and_then(|v| v.as_f64()), Some(4.0));
}

#[serial]
#[test]
fn runtime_host_iterate_plugins_agent_retries_on_warning_and_promotes() {
    // LLM-network coverage: multi-round agent iteration (warning-driven retry
    // loop) over the mock SSE server, exercising `execute_tool` including the
    // warning-recovery re-edit path. No longer linux-gated.
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");

    let lexer_before = read_rel(&fixtures, "plugins/expr/lexer/src/core.rs");
    let lexer_after = replace_once(
        &lexer_before,
        "    Exclamation,\n    LParen,\n",
        "    Exclamation,\n    Tilde,\n    LParen,\n",
    );
    let lexer_after = replace_once(
        &lexer_after,
        "            '!' => {\n                pos += 1;\n                Token {\n                    kind: TokenKind::Exclamation,\n                    position: pos - 1,\n                }\n            }\n            '(' => {\n",
        "            '!' => {\n                pos += 1;\n                Token {\n                    kind: TokenKind::Exclamation,\n                    position: pos - 1,\n                }\n            }\n            '~' => {\n                pos += 1;\n                Token {\n                    kind: TokenKind::Tilde,\n                    position: pos - 1,\n                }\n            }\n            '(' => {\n",
    );

    let parser_before = read_rel(&fixtures, "plugins/expr/parser/src/core.rs");
    let parser_after = replace_once(&parser_before, "    Pow,\n}\n", "    Pow,\n    Dist,\n}\n");
    let parser_after = replace_once(
        &parser_after,
        "                Some(TokenKind::Percent) => BinaryOp::Mod,\n                _ => break,\n",
        "                Some(TokenKind::Percent) => BinaryOp::Mod,\n                Some(TokenKind::Tilde) => BinaryOp::Dist,\n                _ => break,\n",
    );

    let evaluator_before = read_rel(&fixtures, "plugins/expr/evaluator/src/core.rs");
    let evaluator_after = replace_once(
        &evaluator_before,
        "#[path = \"../div/src/core.rs\"]\npub mod div_core;\n",
        "#[path = \"../div/src/core.rs\"]\npub mod div_core;\n#[path = \"../dist/src/core.rs\"]\npub mod dist_core;\n",
    );
    let evaluator_after = replace_once(
        &evaluator_after,
        "pub use div_core::{DivError, DivPlugin};\n",
        "pub use div_core::{DivError, DivPlugin};\npub use dist_core::{DistError, DistPlugin};\n",
    );
    let evaluator_after = replace_once(
        &evaluator_after,
        "    div: DivPlugin,\n",
        "    div: DivPlugin,\n    dist: DistPlugin,\n",
    );
    let evaluator_after = replace_once(
        &evaluator_after,
        "                BinaryOp::Div => ops.div.apply(left, right).map_err(|err| match err {\n                    DivError::DivisionByZero => EvalError::DivisionByZero,\n                }),\n",
        "                BinaryOp::Div => ops.div.apply(left, right).map_err(|err| match err {\n                    DivError::DivisionByZero => EvalError::DivisionByZero,\n                }),\n                BinaryOp::Dist => ops.dist.apply(left, right).map_err(|err| match err {\n                    DistError::NotImplemented => EvalError::NonFinite,\n                }),\n",
    );

    let eval_tests_before = read_rel(&fixtures, "plugins/expr/tests/eval.rs");
    let eval_tests_after = format!(
        "{eval_tests_before}\n#[test]\nfn evaluates_dist_expression() {{\n    let value = evaluate_expression(\"7 ~ 4 + 1\").expect(\"must evaluate\");\n    assert_eq!(value, 4.0);\n}}\n"
    );

    let responses = vec![
        tool_call_response(
            "chatcmpl_plugin_iter_warning_1",
            vec![(
                "call_scaffold_mod",
                "scaffold_child_plugin",
                json!({
                    "parent_plugin_path": "expr/evaluator",
                    "child_name": "dist",
                    "node_id": "expr_dist",
                    "summary": "Compute absolute difference of lhs and rhs."
                }),
            )],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_warning_2",
            vec![
                (
                    "call_replace_lexer",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/lexer/src/core.rs",
                        "expected_old_string": lexer_before,
                        "new_content": lexer_after,
                    }),
                ),
                (
                    "call_replace_parser",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/parser/src/core.rs",
                        "expected_old_string": parser_before,
                        "new_content": parser_after,
                    }),
                ),
                (
                    "call_replace_evaluator",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/evaluator/src/core.rs",
                        "expected_old_string": evaluator_before,
                        "new_content": evaluator_after,
                    }),
                ),
                (
                    "call_replace_mod_core_warning",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/evaluator/dist/src/core.rs",
                        "expected_old_string": generated_dist_scaffold_core(),
                        "new_content": implemented_dist_core_with_warning(),
                    }),
                ),
                (
                    "call_replace_eval_tests",
                    "replace_file_exact",
                    json!({
                        "path": "plugins/expr/tests/eval.rs",
                        "expected_old_string": eval_tests_before,
                        "new_content": eval_tests_after,
                    }),
                ),
            ],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_warning_3",
            vec![(
                "call_run_tests_warning",
                "run_plugin_test",
                json!({
                    "command": "cargo test --quiet --manifest-path plugins/expr/Cargo.toml"
                }),
            )],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_warning_4",
            vec![(
                "call_fix_mod_warning",
                "replace_file_exact",
                json!({
                    "path": "plugins/expr/evaluator/dist/src/core.rs",
                    "expected_old_string": implemented_dist_core_with_warning(),
                    "new_content": implemented_dist_core(),
                }),
            )],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_warning_5",
            vec![(
                "call_run_tests_clean",
                "run_plugin_test",
                json!({
                    "command": "cargo test --quiet --manifest-path plugins/expr/Cargo.toml"
                }),
            )],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_warning_6",
            vec![(
                "call_record_summary",
                "record_iteration_summary",
                json!({
                    "summary": "Add dist child plugin support under expr/evaluator/dist and clean warnings before promotion.",
                    "tests_command": "cargo test --quiet --manifest-path plugins/expr/Cargo.toml"
                }),
            )],
        ),
    ];
    let (base_url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(responses);
    write_llm_api_config(temp.path(), &base_url, 120_000);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    let seed = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "1 + 2 * 3" }).to_string(),
        )
        .expect("expr invoke should seed canary replay");
    let seed_value: Value = serde_json::from_str(&seed.payload).expect("seed response json");
    assert_eq!(seed_value.get("value").and_then(|v| v.as_f64()), Some(7.0));

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["expr".to_string()],
            instruction: Some("Add absolute-difference (~) operator support with a sibling evaluator child plugin at expr/evaluator/dist.".to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect("agent-driven plugin iteration should recover from warnings");

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");

    assert!(requests.len() >= 3);
    assert!(requests.iter().any(|request| {
        request.contains("still emitted warnings in files changed during this iteration")
    }));
    assert!(requests
        .iter()
        .any(|request| request.contains("unused import: `std::fmt`")));
    assert_eq!(result.final_verdict, PluginIterationFinalVerdict::Promoted);

    let core_after = read_rel(&fixtures, "plugins/expr/evaluator/dist/src/core.rs");
    assert_eq!(core_after, implemented_dist_core());
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("expr/evaluator/dist")
        .is_some());
}

#[serial]
#[test]
fn runtime_host_iterate_plugins_agent_retries_on_raw_mod_identifier_and_rolls_back() {
    // LLM-network coverage: this agent-driven iteration exercises the mock
    // SSE server and `PluginIterationAgentBackend::execute_tool` dispatch
    // (scaffold_child_plugin, replace_file_exact). It no longer skips on
    // non-linux hosts: `ensure_fixture_artifacts` (via setup_fixture_*_copy)
    // rebuilds the fixture dylibs for the local target, so a real boot +
    // agent loop succeeds on arm64 macOS too.
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let evaluator_before = read_rel(&fixtures, "plugins/expr/evaluator/src/core.rs");
    let evaluator_bad = replace_once(
        &evaluator_before,
        "    div: DivPlugin,\n",
        "    div: DivPlugin,\n    mod: ModPlugin,\n",
    );

    let responses = vec![
        tool_call_response(
            "chatcmpl_plugin_iter_bad_1",
            vec![(
                "call_scaffold_mod",
                "scaffold_child_plugin",
                json!({
                    "parent_plugin_path": "expr/evaluator",
                    "child_name": "mod",
                    "node_id": "expr_mod",
                    "summary": "Compute lhs modulo rhs."
                }),
            )],
        ),
        tool_call_response(
            "chatcmpl_plugin_iter_bad_2",
            vec![(
                "call_replace_bad_evaluator",
                "replace_file_exact",
                json!({
                    "path": "plugins/expr/evaluator/src/core.rs",
                    "expected_old_string": evaluator_before,
                    "new_content": evaluator_bad,
                }),
            )],
        ),
        assistant_response("chatcmpl_plugin_iter_bad_3", "The change is complete."),
    ];
    let (base_url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(responses);
    write_llm_api_config(temp.path(), &base_url, 120_000);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["expr".to_string()],
            instruction: Some("Add absolute-difference (~) operator support with a sibling evaluator child plugin at expr/evaluator/dist.".to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect("agent-driven failure should still return rollback result");

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    assert!(result
        .blocked_reason
        .as_deref()
        .unwrap_or_default()
        .contains("record_iteration_summary"));
    assert!(host.candidate_snapshot().is_none());
    assert!(requests.len() >= 3);
    assert!(requests[2].contains("expr/evaluator/mod"));
    assert!(requests[2].contains("raw Rust identifier `mod` is invalid"));
    assert!(!fixtures
        .join("plugins/expr/evaluator/mod/Cargo.toml")
        .exists());
}

#[test]
fn runtime_host_iterate_plugins_blocks_without_canary_evidence_and_approve_promotes() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let journal_path = plugin_iteration_journal_path(&host.status().snapshot_root);
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
    assert!(journal_path.exists());
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
    assert!(!journal_path.exists());
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let journal_path = plugin_iteration_journal_path(&host.status().snapshot_root);
    // 5.2.27: previously targeted the `root` fixture's `./child` declaration,
    // but P2-10 removed root/child (root now has `children = []` and is not
    // even a workspace member). Break the expr tree's `./lexer` child source
    // instead — same semantics: an edit that corrupts a parent manifest must
    // roll back and leave the runtime alive.
    let manifest_path = fixtures.join("plugins/expr/Cargo.toml");
    let original_manifest = fs::read_to_string(&manifest_path).expect("read expr manifest");

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["expr".to_string()],
            instruction: Some("break expr child source".to_string()),
            edit_plan: Some(PluginEditPlan {
                issue_id: "issue-expr-manifest".to_string(),
                patch_id: "patch-expr-manifest".to_string(),
                summary: "break expr child source".to_string(),
                operations: vec![PluginEditOperation {
                    path: "plugins/expr/Cargo.toml".to_string(),
                    kind: PluginEditOpKind::ReplaceExact,
                    expected_old_string: Some("./lexer".to_string()),
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
    assert!(!journal_path.exists());
    assert_eq!(
        fs::read_to_string(&manifest_path).expect("manifest should be restored"),
        original_manifest
    );
    assert!(host.kernel().plugin_issues().iter().any(|issue| {
        issue.root_plugin_path == "expr" && issue.source == KernelPluginIssueSource::LoadFailure
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
fn runtime_host_rollback_candidate_restores_plugin_sources_and_clears_journal() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let journal_path = plugin_iteration_journal_path(&host.status().snapshot_root);
    let source_path = fixtures.join("plugins/shell/src/lib.rs");
    let original_source = fs::read_to_string(&source_path).expect("read shell source");
    let original_summary =
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry");
    let updated_summary = format!("{original_summary} (rollback candidate)");

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["shell".to_string()],
            instruction: Some("update shell docs summary without replay evidence".to_string()),
            edit_plan: Some(PluginEditPlan {
                issue_id: "issue-shell-rollback".to_string(),
                patch_id: "patch-shell-rollback".to_string(),
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
        .expect("plugin iteration should enter blocked state");

    assert_eq!(result.final_verdict, PluginIterationFinalVerdict::Blocked);
    assert!(journal_path.exists());
    assert!(fs::read_to_string(&source_path)
        .expect("read updated shell source")
        .contains(&updated_summary));

    host.rollback_candidate()
        .expect("manual rollback should discard candidate and restore sources");

    assert!(host.candidate_snapshot().is_none());
    assert!(!journal_path.exists());
    assert_eq!(
        fs::read_to_string(&source_path).expect("shell source should be restored"),
        original_source
    );
    assert_eq!(
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry"),
        original_summary
    );
}

#[test]
fn runtime_host_boot_recovers_plugin_iteration_journal() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let journal_path = plugin_iteration_journal_path(&host.status().snapshot_root);
    let source_path = fixtures.join("plugins/shell/src/lib.rs");
    let original_source = fs::read_to_string(&source_path).expect("read shell source");
    let original_summary =
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry");
    let updated_summary = format!("{original_summary} (recovery candidate)");

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["shell".to_string()],
            instruction: Some("update shell docs summary without replay evidence".to_string()),
            edit_plan: Some(PluginEditPlan {
                issue_id: "issue-shell-recovery".to_string(),
                patch_id: "patch-shell-recovery".to_string(),
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
        .expect("plugin iteration should enter blocked state");

    assert_eq!(result.final_verdict, PluginIterationFinalVerdict::Blocked);
    assert!(journal_path.exists());
    assert!(fs::read_to_string(&source_path)
        .expect("read updated shell source")
        .contains(&updated_summary));

    drop(host);

    let recovered = RuntimeHost::boot(&fixtures).expect("host should recover and boot");
    assert!(recovered.candidate_snapshot().is_none());
    assert!(!journal_path.exists());
    assert_eq!(
        fs::read_to_string(&source_path).expect("shell source should be restored"),
        original_source
    );
    assert_eq!(
        plugin_node_summary(
            recovered.current_snapshot().as_ref(),
            "shell",
            "shell_entry"
        ),
        original_summary
    );
}

#[serial]
#[test]
fn serve_mode_supports_plugins_reload_and_kernel_status() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let mut child = Command::new(bin)
        .args(["serve", fixtures.to_str().expect("temp path utf-8")])
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

// 5.2.27-2 后续：原测试用手工拼进 index.json 的 demo process 插件做
// candidate invoke，依赖 --runtime-only 跳过 prepare_artifacts 的旧行为。
// 去掉 --runtime-only 后 serve 启动会跑 prepare 重建 index，手工 demo
// 条目的 `execution` 字段在重建中丢失 → "plugin execution unsupported"。
// 测试目的（candidate 控制面命令：status/reload/invoke/promote 的往返）
// 与 process 插件无关，改用真实 expr dylib 插件走一遍。
#[serial]
#[test]
fn serve_mode_supports_candidate_control_plane() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let mut child = Command::new(bin)
        .args(["serve", fixtures.to_str().expect("temp path utf-8")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve cli");

    let stdin = child.stdin.as_mut().expect("stdin pipe");
    use std::io::Write as _;
    stdin
        .write_all(
            b"candidate status\ncandidate reload\ncandidate status\ncandidate invoke expr expr_entry {\"expression\":\"2 + 3\"}\ncandidate promote\nstatus\ncandidate status\nexit\n",
        )
        .expect("write serve candidate commands");

    let output = child.wait_with_output().expect("wait for serve cli");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(
        stdout.contains("serve ready snapshot_id="),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("null"), "stdout: {stdout}");
    assert!(stdout.contains("\"status\":\"staged\""), "stdout: {stdout}");
    assert!(
        stdout.contains("\"candidate_snapshot_id\""),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("\"value\":5.0"), "stdout: {stdout}");
    assert!(
        stdout.contains("\"current_snapshot_id\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"candidate_snapshot\":null"),
        "stdout: {stdout}"
    );
}

// L批: LLM profile fallback 状态机。default 指向死端口 → 自动切 fallback
// 'fast'（mock server）并记录 kernel issue；default 恢复后乐观探测切回。
fn write_llm_profiles_config(root: &Path, default_url: &str, fast_url: &str) {
    // provider 插件在请求时从环境读 key（契约类型不带明文 key）。
    std::env::set_var("CORDIS_TEST_LLM_KEY", "test-key");
    let config_dir = root.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "profiles:\n  default:\n    provider: deepseek\n    base_url: {default_url}\n    api_key_env: CORDIS_TEST_LLM_KEY\n    model: deepseek-reasoner\n    timeout_ms: 10000\n    fallback: fast\n  fast:\n    provider: deepseek\n    base_url: {fast_url}\n    api_key_env: CORDIS_TEST_LLM_KEY\n    model: deepseek-chat\n    timeout_ms: 10000\n"
        ),
    )
    .expect("write llm profiles config");
}

/// Serve exactly one SSE chat completion on an already-bound listener.
fn serve_one_sse(
    listener: std::net::TcpListener,
    chunks: Vec<(u64, String)>,
) -> std::thread::JoinHandle<usize> {
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Read as _, Write as _};
        let (mut stream, _) = listener.accept().expect("accept probe request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("request line");
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("header line");
            if header == "\r\n" {
                break;
            }
            if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().expect("content length");
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).expect("request body");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .expect("headers");
        for (delay_ms, chunk) in &chunks {
            std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
            write!(stream, "{:X}\r\n{}\r\n", chunk.len(), chunk).expect("chunk");
        }
        write!(stream, "0\r\n\r\n").expect("chunked end");
        stream.flush().expect("flush");
        1
    })
}

#[test]
#[serial]
fn llm_profile_fallback_degrades_and_recovers() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    use cordis_runtime::host::{AgentSessionKind, AgentStartOptions};

    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");

    // Reserve a port for "default" then free it so the first send fails.
    let placeholder = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let default_addr = placeholder.local_addr().expect("addr");
    drop(placeholder);
    let default_url = format!("http://{default_addr}/v1");

    let (fast_url, fast_requests_rx, fast_handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_response(
            "fallback_reply",
            "served by fast",
        )]);
    write_llm_profiles_config(temp.path(), &default_url, &fast_url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let handle = host
        .agent_start_with(
            AgentSessionKind::RuntimeShell,
            AgentStartOptions {
                profile: Some("default".to_string()),
                ..Default::default()
            },
        )
        .expect("agent start");
    let sid = handle.session_id.as_str();

    // 1) default dead → degrade to fast, reply still succeeds.
    let reply = host
        .agent_send_with_fallback(sid, "hello")
        .expect("fallback should rescue the request");
    assert!(
        reply.content.contains("served by fast"),
        "content: {}",
        reply.content
    );
    let issues = host.kernel().plugin_issues();
    assert!(
        issues.iter().any(|i| i.root_plugin_path == "/llm-profile"
            && i.summary.contains("degraded to fallback 'fast'")),
        "issues: {issues:?}"
    );
    let fast_requests = fast_requests_rx.recv().expect("fast requests");
    fast_handle.join().expect("join fast mock");
    assert_eq!(fast_requests.len(), 1);

    // 2) default comes back → optimistic probe switches back on next send.
    let revived = std::net::TcpListener::bind(default_addr).expect("rebind default port");
    let probe_handle = serve_one_sse(
        revived,
        assistant_response("recovered_reply", "served by default"),
    );
    let reply = host
        .agent_send_with_fallback(sid, "are you back?")
        .expect("recovered profile should serve");
    assert!(
        reply.content.contains("served by default"),
        "content: {}",
        reply.content
    );
    assert_eq!(probe_handle.join().expect("join probe server"), 1);
}

// O批: soul 槽 — set_soul 经 host 写入文件 provider，新会话 system prompt
// 含 persona overlay；soul.profile 引用决定新会话使用的 LLM profile。
#[test]
#[serial]
fn soul_roundtrip_profile_reference_and_scope_guard() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    use cordis_runtime::agent::AgentToolHost;
    use cordis_runtime::soul::Soul;

    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    write_llm_profiles_config(
        temp.path(),
        "http://127.0.0.1:1/v1",
        "http://127.0.0.1:2/v1",
    );
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    let soul_key = "feishu:ou_abc#private";
    // agent 工具写路径（merge 语义：先 persona 后 profile 不互相覆盖）。
    host.agent_set_soul(soul_key, Some("你是毒舌但可靠的运维助手"), None)
        .expect("set persona");
    host.agent_set_soul(soul_key, None, Some("fast"))
        .expect("set profile");
    let soul: Soul = host.get_soul(soul_key).expect("get").expect("exists");
    assert_eq!(soul.persona, "你是毒舌但可靠的运维助手");
    assert_eq!(soul.profile.as_deref(), Some("fast"));

    // 未知 profile 拒绝。
    let err = host
        .agent_set_soul(soul_key, None, Some("ghost"))
        .expect_err("unknown profile must fail");
    assert!(err.to_string().contains("ghost"), "err: {err}");

    // 空 soul_key（无身份会话）拒绝写入。
    assert!(host.agent_set_soul("", Some("x"), None).is_err());

    // overlay 读路径。
    assert_eq!(
        host.agent_soul_overlay(soul_key).as_deref(),
        Some("你是毒舌但可靠的运维助手")
    );
    assert!(host.agent_soul_overlay("nobody#private").is_none());
}

// P批: soul_store 插件加载后，soul 读写应走 SQLite 覆写（写入
// data/souls.db 而非 data/souls/*.json）；这是"约定能力节点"覆写
// 路径的端到端验证。
#[test]
#[serial]
fn soul_store_plugin_overrides_file_provider() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    use cordis_runtime::agent::AgentToolHost;

    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    write_llm_profiles_config(
        temp.path(),
        "http://127.0.0.1:1/v1",
        "http://127.0.0.1:2/v1",
    );
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    // No load-status guard here on purpose: soul_store is a required,
    // committed fixture. If it ever fails to load, the assertions below
    // must go red (a silent fallback to FileSoulProvider would create
    // souls/*.json instead of souls.db), not skip.
    let soul_key = "qq:789#group";
    host.agent_set_soul(soul_key, Some("SQLite 里的我"), None)
        .expect("set soul via plugin provider");
    let soul = host.get_soul(soul_key).expect("get").expect("exists");
    assert_eq!(soul.persona, "SQLite 里的我");

    // 覆写生效的证据：文件 provider 的目录不应有这个 key 的文件。
    let file_path = temp.path().join("data/souls").join("qq_789#group.json");
    assert!(
        !file_path.exists(),
        "soul must live in souls.db, not {}",
        file_path.display()
    );
    assert!(
        temp.path().join("data/souls.db").exists(),
        "souls.db should exist"
    );
}

// H1: 群聊 soul 错位修复。session 的 soul_key 由 inbox 随最近发言者刷新
// (refresh_session_soul);persona overlay 每轮从 session.soul_key 重建,
// 刷新后 system prompt 立刻对齐新的人。
#[test]
#[serial]
fn refresh_session_soul_switches_persona_scope() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    use cordis_runtime::agent::AgentToolHost;
    use cordis_runtime::host::{AgentSessionKind, AgentStartOptions};

    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");

    // Two turns served by the same "default" mock server; the persona in
    // each request body is what we assert on.
    let (default_url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(vec![
        assistant_response("soul_switch_1", "回复给 A"),
        assistant_response("soul_switch_2", "回复给 B"),
    ]);
    // fast profile is a dead port; unused here (no fallback path exercised).
    write_llm_profiles_config(temp.path(), &default_url, "http://127.0.0.1:2/v1");

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    // Two distinct souls in the same group scope.
    host.agent_set_soul("userA#group", Some("A 的人格"), None)
        .expect("set soul A");
    host.agent_set_soul("userB#group", Some("B 的人格"), None)
        .expect("set soul B");

    // Session starts bound to A's soul scope.
    let handle_session = host
        .agent_start_with(
            AgentSessionKind::RuntimeShell,
            AgentStartOptions {
                profile: None,
                soul_key: "userA#group".to_string(),
            },
        )
        .expect("agent start");
    let sid = handle_session.session_id.clone();

    // Turn 1: persona overlay should be A's.
    host.agent_send(&sid, "hi from A").expect("first send");

    // Inbox refreshes the session soul to the latest speaker (B).
    host.refresh_session_soul(&sid, "userB#group")
        .expect("refresh to B");

    // Turn 2: persona overlay rebuilt from the new soul_key → B's.
    host.agent_send(&sid, "hi from B").expect("second send");

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");

    assert_eq!(requests.len(), 2, "expected exactly two turns");
    assert!(
        requests[0].contains("A 的人格"),
        "first turn should carry A's persona: {}",
        requests[0]
    );
    assert!(
        requests[1].contains("B 的人格"),
        "second turn should carry B's persona after refresh: {}",
        requests[1]
    );
    assert!(
        !requests[1].contains("A 的人格"),
        "persona overlay is rebuilt each turn; A must be gone after refresh: {}",
        requests[1]
    );

    // Unknown session id must error, not silently no-op.
    assert!(
        host.refresh_session_soul("no-such-sid", "userA#group")
            .is_err(),
        "refresh on unknown session must be an error"
    );
}

// H2: session 内存/磁盘终结清理。drop_session 清 agent_sessions /
// pending_session_actions / profile_fallback 三张 map + 删除磁盘快照,
// 且幂等。debug_session_map_sizes 暴露三张 map 的大小用于断言。
#[test]
#[serial]
fn drop_session_evicts_memory_and_disk() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    use cordis_runtime::host::{AgentSessionKind, AgentStartOptions};

    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");

    // Both sessions send one turn each so a disk snapshot exists (auto_save
    // fires on successful send, not at start).
    let (default_url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(vec![
        assistant_response("drop_1", "ok one"),
        assistant_response("drop_2", "ok two"),
    ]);
    write_llm_profiles_config(temp.path(), &default_url, "http://127.0.0.1:2/v1");

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    let s1 = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start s1")
        .session_id;
    let s2 = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start s2")
        .session_id;

    // Send one turn each so each session lands a disk snapshot.
    host.agent_send(&s1, "hello one").expect("send s1");
    host.agent_send(&s2, "hello two").expect("send s2");
    let captured = requests_rx.recv().expect("captured requests");
    assert_eq!(
        captured.len(),
        2,
        "both scripted responses must have been dialled"
    );
    handle.join().expect("join mock server");

    // agent_sessions=2, pending_session_actions=0, profile_fallback=2.
    assert_eq!(
        host.debug_session_map_sizes(),
        (2, 0, 2),
        "two live sessions across the tracked maps"
    );

    let s1_snapshot = temp.path().join("data/sessions").join(format!("{s1}.json"));
    assert!(s1_snapshot.exists(), "s1 snapshot should exist after send");

    // Drop the first session: memory maps shrink and disk snapshot removed.
    host.drop_session(&s1);
    assert_eq!(
        host.debug_session_map_sizes(),
        (1, 0, 1),
        "dropping one session frees exactly one slot in each populated map"
    );
    assert!(
        !s1_snapshot.exists(),
        "drop_session must delete the on-disk snapshot"
    );

    // A dropped session id is gone from the query surface.
    assert!(
        matches!(
            host.agent_status(&s1),
            Err(cordis_runtime::core::error::RuntimeError::AgentSessionNotFound { .. })
        ),
        "agent_status on dropped session must be AgentSessionNotFound"
    );
    assert!(
        matches!(
            host.agent_transcript(&s1),
            Err(cordis_runtime::core::error::RuntimeError::AgentSessionNotFound { .. })
        ),
        "agent_transcript on dropped session must be AgentSessionNotFound"
    );

    // The surviving session is untouched.
    assert!(host.agent_status(&s2).is_ok(), "s2 should still be live");

    // Idempotent: dropping again is a no-op, not a panic or double-free.
    host.drop_session(&s1);
    assert_eq!(
        host.debug_session_map_sizes(),
        (1, 0, 1),
        "dropping the same session twice is idempotent"
    );
}

// review 欠账: command_router::dispatch 表驱动覆盖。bypass-LLM 指令路由的
// 各分支(内建 + 未知)在真实 boot 的运行时上逐一验证。
#[test]
#[serial]
fn command_router_dispatch_table() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    use cordis_runtime::agent::AgentToolHost;
    use cordis_runtime::command_router::{dispatch, CommandContext, CommandOutcome};
    use CommandOutcome::Reply;

    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    write_llm_profiles_config(
        temp.path(),
        "http://127.0.0.1:1/v1",
        "http://127.0.0.1:2/v1",
    );
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    // Seed a soul so /soul reads back a concrete persona for this scope.
    let soul_key = "feishu:ou_cmd#private";
    host.agent_set_soul(soul_key, Some("表驱动测试人格"), None)
        .expect("seed soul for /soul");

    let ctx = CommandContext {
        session_key: "sess-cmd".to_string(),
        sender_id: "ou_cmd".to_string(),
        conversation_kind: "private".to_string(),
        soul_key: soul_key.to_string(),
    };

    // (input, predicate on the Reply text) for the Reply-returning cases.
    type ReplyPredicate = Box<dyn Fn(&str) -> bool>;
    let reply_cases: Vec<(&str, ReplyPredicate)> = vec![
        (
            "/status",
            Box::new(|t: &str| t.contains("运行时状态") && t.contains("snapshot")),
        ),
        (
            "/help",
            Box::new(|t: &str| {
                t.contains("可用指令") && t.contains("/status") && t.contains("/soul")
            }),
        ),
        (
            "/soul",
            Box::new(move |t: &str| t.contains("表驱动测试人格") && t.contains(soul_key)),
        ),
        (
            "/nonexistent",
            Box::new(|t: &str| t.contains("未知指令") && t.contains("/nonexistent")),
        ),
    ];
    for (input, ok) in reply_cases {
        let out = dispatch(&host, &ctx, input);
        let Reply(text) = out else { panic!("{out:?}") };
        assert!(ok(&text), "dispatch({input}) reply mismatch: {text}");
    }

    // /reset is the one non-Reply outcome.
    assert!(
        matches!(
            dispatch(&host, &ctx, "/reset"),
            CommandOutcome::ResetSession(_)
        ),
        "/reset must yield ResetSession"
    );

    // /soul with an empty soul_key (identity-less session) falls back to the
    // "no identity" message instead of leaking another user's persona.
    let anon = CommandContext::default();
    let out = dispatch(&host, &anon, "/soul");
    let Reply(text) = out else { panic!("{out:?}") };
    assert!(
        text.contains("没有身份") || text.contains("无法定位"),
        "/soul without soul_key should explain the missing identity: {text}"
    );
    assert!(
        !text.contains("表驱动测试人格"),
        "/soul without soul_key must not leak another scope's persona"
    );
}

// ---------------------------------------------------------------------------
// R1: agent_send 摘除-重插窗口（并发时序测试）
// ---------------------------------------------------------------------------

/// R1: 一个接受连接后先发 `started` 信号、再等 `release` 信号才回 SSE
/// 补全的 mock server。让测试能精确控制 `agent_send` 的 respond 处于
/// in-flight 的时长（期间会话在 map 外、inflight 标记在集合里），从而
/// 可靠地插入 drop_session / 并发第二个 agent_send。
///
/// 返回 (base_url, started_rx, release_tx, join_handle)：测试等 started_rx
/// 收到信号即可认为 respond 已进入 in-flight，随后做并发操作，最后发
/// release_tx 放行补全。
fn spawn_blocking_mock_llm_server(
    chunks: Vec<(u64, String)>,
) -> (
    String,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    use std::io::{BufRead, BufReader, Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind blocking server");
    let addr = listener.local_addr().expect("addr");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("request line");
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("header line");
            if header == "\r\n" {
                break;
            }
            if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().expect("content length");
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).expect("request body");
        // respond 已真正进入 in-flight：会话已从 map 摘除、inflight 已标记。
        started_tx.send(()).expect("signal started");
        // 等测试释放（期间测试可以 drop_session / 并发 agent_send）。
        release_rx.recv().expect("release signal");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .expect("headers");
        for (delay_ms, chunk) in &chunks {
            std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
            write!(stream, "{:X}\r\n{}\r\n", chunk.len(), chunk).expect("chunk");
        }
        write!(stream, "0\r\n\r\n").expect("chunked end");
        stream.flush().expect("flush");
    });
    (
        format!("http://{addr}/v1"),
        started_rx,
        release_tx,
        handle,
    )
}

/// R1(b): respond 期间 drop_session —— 会话在 map 里已不存在（被摘除），
/// 旧的 drop 只删快照；turn 结束后不得被插回复活、不得 auto_save 重写
/// 快照。
#[test]
#[serial]
fn agent_send_drop_during_turn_does_not_resurrect_session_or_snapshot() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    use cordis_runtime::host::{AgentSessionKind, AgentStartOptions};

    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let (url, started_rx, release_tx, handle) =
        spawn_blocking_mock_llm_server(assistant_response("blocked_1", "ok"));
    write_llm_profiles_config(temp.path(), &url, &url);

    let host = std::sync::Arc::new(RuntimeHost::boot(&fixtures).expect("host should boot"));
    let sid = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start session")
        .session_id;

    // 线程 1：agent_send，respond 阻塞在 mock server 上。
    let sender = std::sync::Arc::clone(&host);
    let send_sid = sid.clone();
    let send_thread = std::thread::spawn(move || sender.agent_send(&send_sid, "hello"));

    // 等请求到达 → respond 进入 in-flight（会话不在 map）。
    started_rx.recv().expect("request arrived");

    // 期间调用 drop_session：会话本尊不在 map，必须标记 dropped_during_turn
    // 而不是只删掉快照了事。
    host.drop_session(&sid);

    // 放行 server → respond 完成 → 收尾消费标记：不插回、不 auto_save。
    release_tx.send(()).expect("release server");
    let reply = send_thread
        .join()
        .expect("join send thread")
        .expect("the in-flight send itself succeeds");
    assert!(reply.content.contains("ok"), "content: {}", reply.content);

    // 会话没有被复活。
    assert!(
        matches!(
            host.agent_status(&sid),
            Err(cordis_runtime::core::error::RuntimeError::AgentSessionNotFound { .. })
        ),
        "session must not be resurrected after drop-during-turn"
    );
    // 快照没有被 auto_save 重写（turn 是成功的——若复活逻辑漏了就会重写）。
    let snapshot = temp.path().join("data/sessions").join(format!("{sid}.json"));
    assert!(!snapshot.exists(), "snapshot must not be rewritten after drop");
    // drop_session 的既有语义：三张 per-session map 全清。
    assert_eq!(host.debug_session_map_sizes(), (0, 0, 0));
    handle.join().expect("join mock server");
}

/// R1(c): 并发第二个 agent_send 得到 `AgentSessionBusy`，而不是误报
/// `AgentSessionNotFound`（旧实现摘除后消息静默丢失）。
#[test]
#[serial]
fn agent_send_concurrent_second_send_returns_busy_not_not_found() {
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
    use cordis_runtime::host::{AgentSessionKind, AgentStartOptions};

    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let (url, started_rx, release_tx, handle) =
        spawn_blocking_mock_llm_server(assistant_response("blocked_2", "ok"));
    write_llm_profiles_config(temp.path(), &url, &url);

    let host = std::sync::Arc::new(RuntimeHost::boot(&fixtures).expect("host should boot"));
    let sid = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start session")
        .session_id;

    let sender = std::sync::Arc::clone(&host);
    let send_sid = sid.clone();
    let send_thread = std::thread::spawn(move || sender.agent_send(&send_sid, "hello"));
    started_rx.recv().expect("request arrived");

    // 会话在 map 外（inflight）：第二个 send 必须报 Busy 而不是 NotFound。
    let err = host
        .agent_send(&sid, "second")
        .expect_err("concurrent second send must be rejected");
    assert!(
        matches!(
            err,
            cordis_runtime::core::error::RuntimeError::AgentSessionBusy { ref session_id }
                if session_id == &sid
        ),
        "expected AgentSessionBusy, got {err:?}"
    );

    // 放行后第一个 turn 正常完成，会话仍在、可继续对话。
    release_tx.send(()).expect("release server");
    let reply = send_thread
        .join()
        .expect("join send thread")
        .expect("first send succeeds");
    assert!(reply.content.contains("ok"), "content: {}", reply.content);
    assert!(host.agent_status(&sid).is_ok(), "session must still be live");
    handle.join().expect("join mock server");
}
