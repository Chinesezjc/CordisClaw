//! Coverage-closing tests for `RuntimeHost`'s `AgentToolHost` implementation.
//!
//! One test per tool family, driven through the real trait on a `TempDir`-backed
//! workspace: no test reads or writes the repository's `fixtures/` or `data/`,
//! and every fixture plugin is a JSON artifact, so nothing shells out to cargo
//! to build a dylib.
//!
//! Error text is asserted byte for byte, the same discipline the rest of the
//! crate's host tests use.

use crate::agent_api::{AgentBackend, AgentToolHost};
use crate::agent_tools::{
    filter_kernel_introspection_messages, invoke_payload_error, io_error_at,
    parse_tool_value_arguments, to_json_value, ReadFileArgs,
};
use crate::core::error::RuntimeError;
use crate::core::models::{PluginArtifact, PluginExecution};
use crate::host::{AgentSessionKind, AgentStartOptions, RuntimeHost};
use cordis_plugin_sdk::{AbiFingerprint, PluginDocs};
use serde_json::{json, Value};
use serial_test::serial;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ───────────────────────────── workspace scaffold ─────────────────────────────

/// A bare workspace: `fixtures/artifacts/` and a `config/runtime.yaml` whose
/// snapshot root is pinned inside the `TempDir`, so boot never touches the
/// repository and never piles snapshots into the global temp dir.
fn workspace() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    fs::create_dir_all(fixtures.join("artifacts")).expect("artifacts dir");
    let config = temp.path().join("config");
    fs::create_dir_all(&config).expect("config dir");
    fs::write(
        config.join("runtime.yaml"),
        format!(
            "runtime:\n  snapshot_root: {}\n",
            temp.path().join("snapshots").display()
        ),
    )
    .expect("runtime.yaml");
    (temp, fixtures)
}

fn write_index(fixtures: &Path, entries: Value) {
    fs::write(
        fixtures.join("artifacts/index.json"),
        serde_json::to_string_pretty(&json!({
            "schema_version": 2,
            "generated_at": "2026-07-24T00:00:00Z",
            "topo_order": entries
                .as_array()
                .expect("entries array")
                .iter()
                .map(|entry| entry["plugin_path"].clone())
                .collect::<Vec<_>>(),
            "entries": entries,
        }))
        .expect("index json"),
    )
    .expect("write artifact index");
}

fn empty_host() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = workspace();
    write_index(&fixtures, json!([]));
    let host = RuntimeHost::boot(&fixtures).expect("boot on an empty index");
    (temp, host)
}

fn demo_docs() -> PluginDocs {
    let mut echo = cordis_plugin_sdk::node_doc(
        "echo",
        "echoes the payload back",
        json!({"type": "object", "properties": {}}),
        json!({"type": "object", "properties": {}}),
        &[],
        &[],
    );
    echo.agent_accessible = true;
    let hidden = cordis_plugin_sdk::node_doc(
        "hidden",
        "not exposed to the agent",
        json!({"type": "object", "properties": {}}),
        json!({"type": "object", "properties": {}}),
        &[],
        &[],
    );
    cordis_plugin_sdk::plugin_docs(
        "demo",
        "demo",
        "0.1.0",
        None,
        vec![echo, hidden],
        Some("demo hint"),
    )
}

fn demo_abi() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_demo_v1", "api_v2")
}

/// A workspace with the Kernel's builtin `cordis` plugin plus two JSON
/// artifacts:
///
/// - `demo`: loaded, with an agent-accessible `echo` node whose process
///   execution is `/bin/cat`, so `invoke_plugin` really runs a process plugin
///   and echoes its payload back; `hidden` is declared but not accessible.
/// - `plain`: loaded, with no nodes and no system hint.
///
/// `host_with_unavailable_plugin` then flips `plain` to `Unavailable`, which is
/// the only way to observe a registry row with `docs: None` — `boot` refuses a
/// workspace whose index already contains an unavailable plugin.
fn json_plugin_host() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = workspace();
    let artifacts = fixtures.join("artifacts");
    let docs = demo_docs();
    let abi = demo_abi();
    let artifact = PluginArtifact {
        plugin_path: "demo".to_string(),
        abi_fingerprint: abi.clone(),
        docs: docs.clone(),
        exports: Vec::new(),
        execution: Some(PluginExecution::Process {
            command: "/bin/cat".to_string(),
            args: Vec::new(),
        }),
    };
    let artifact_path = artifacts.join("demo.json");
    fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&artifact).expect("artifact json"),
    )
    .expect("write demo artifact");
    let sha = crate::plugin::artifact::sha256_file(&artifact_path).expect("sha256");

    let plain_docs =
        cordis_plugin_sdk::plugin_docs("plain", "plain", "0.1.0", None, Vec::new(), None);
    let plain_artifact = PluginArtifact {
        plugin_path: "plain".to_string(),
        abi_fingerprint: demo_abi(),
        docs: plain_docs.clone(),
        exports: Vec::new(),
        execution: None,
    };
    let plain_path = artifacts.join("plain.json");
    fs::write(
        &plain_path,
        serde_json::to_string_pretty(&plain_artifact).expect("plain artifact json"),
    )
    .expect("write plain artifact");
    let plain_sha = crate::plugin::artifact::sha256_file(&plain_path).expect("plain sha256");

    write_index(
        &fixtures,
        json!([
            {
                "plugin_path": "demo",
                "version": "0.1.0",
                "abi_fingerprint": abi,
                "artifact_path": "demo.json",
                "sha256": sha,
                "built_at": "0",
                "parent": null,
                "required": true,
                "grants_from_parent": [],
                "docs": docs,
                "exports": [],
                "execution": {"kind": "process", "command": "/bin/cat"},
                "artifact_kind": "json",
                "build_fingerprint": "bf",
            },
            {
                "plugin_path": "plain",
                "version": "0.1.0",
                "abi_fingerprint": demo_abi(),
                "artifact_path": "plain.json",
                "sha256": plain_sha,
                "built_at": "0",
                "parent": null,
                "required": true,
                "grants_from_parent": [],
                "docs": plain_docs,
                "exports": [],
                "artifact_kind": "json",
                "build_fingerprint": "bf",
            }
        ]),
    );
    let host = RuntimeHost::boot(&fixtures).expect("boot on the demo index");
    (temp, host)
}

/// [`json_plugin_host`] with `plain` flipped to `Unavailable` after boot: the
/// row keeps its slot but loses its docs, which is what the `docs: None`
/// branches of the introspection tools key on.
fn host_with_unavailable_plugin() -> (TempDir, RuntimeHost) {
    let (temp, host) = json_plugin_host();
    host.current_snapshot().plugin_registry().mark_unavailable(
        "plain",
        crate::core::models::PluginUnavailableReason::InitFailed,
    );
    (temp, host)
}

fn start_session(host: &RuntimeHost) -> String {
    host.agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start a session")
        .session_id
}

// ───────────────────────── pure helpers (no host) ────────────────────────────

fn assistant(tool_calls: Option<Value>, content: &str) -> Value {
    let mut message = json!({"role": "assistant", "content": content});
    if let Some(calls) = tool_calls {
        message["tool_calls"] = calls;
    }
    message
}

fn call(id: &str, name: &str) -> Value {
    json!({"id": id, "type": "function", "function": {"name": name, "arguments": "{}"}})
}

#[test]
fn filter_keeps_conversational_messages_untouched() {
    let messages = vec![
        json!({"role": "user", "content": "hi"}),
        // Assistant without `tool_calls` at all.
        assistant(None, "hello"),
        // Assistant with an explicitly empty `tool_calls` array.
        assistant(Some(json!([])), "still hello"),
        // Only non-introspection calls: the whole message survives.
        assistant(Some(json!([call("a", "read_file")])), "reading"),
        // A `tool` result with no matching dropped id survives.
        json!({"role": "tool", "tool_call_id": "a", "content": "ok"}),
        // A `tool` message with no id at all: the `if let Some(tid)` guard is
        // skipped and the message still survives.
        json!({"role": "tool", "content": "no id"}),
    ];
    let out = filter_kernel_introspection_messages(&messages);
    assert_eq!(out, messages);
}

#[test]
fn filter_drops_pure_introspection_turns_and_their_results() {
    let messages = vec![
        // Pure introspection with no text: dropped entirely.
        assistant(Some(json!([call("k1", "get_runtime_status")])), ""),
        // Its result is dropped because the call id was recorded.
        json!({"role": "tool", "tool_call_id": "k1", "content": "{}"}),
        // Pure introspection that also carries text: tool_calls stripped,
        // the message itself kept.
        assistant(Some(json!([call("k2", "list_nodes")])), "  listing  "),
        // No matching id: kept.
        json!({"role": "tool", "tool_call_id": "other", "content": "{}"}),
    ];
    let out = filter_kernel_introspection_messages(&messages);
    assert_eq!(out.len(), 2, "out={out:?}");
    assert_eq!(out[0]["content"], "  listing  ");
    assert!(
        out[0].get("tool_calls").is_none(),
        "dangling tool_calls must be stripped: {out:?}"
    );
    assert_eq!(out[1]["tool_call_id"], "other");
}

#[test]
fn filter_shrinks_mixed_tool_call_batches() {
    let messages = vec![
        assistant(
            Some(json!([
                call("k1", "get_kernel_status"),
                call("r1", "read_file"),
                call("k2", "list_plugins"),
            ])),
            "mixed",
        ),
        json!({"role": "tool", "tool_call_id": "k1", "content": "{}"}),
        json!({"role": "tool", "tool_call_id": "r1", "content": "kept"}),
        json!({"role": "tool", "tool_call_id": "k2", "content": "{}"}),
    ];
    let out = filter_kernel_introspection_messages(&messages);
    assert_eq!(out.len(), 2, "out={out:?}");
    let kept_calls = out[0]["tool_calls"].as_array().expect("shrunk calls");
    assert_eq!(kept_calls.len(), 1);
    assert_eq!(kept_calls[0]["id"], "r1");
    assert_eq!(out[1]["tool_call_id"], "r1");
}

#[test]
fn a_tool_call_without_an_id_is_still_recognised_as_introspection() {
    // No `id` field: nothing to record for the result-dropping pass, but the
    // call still counts as introspection, so a text-only turn is kept with its
    // `tool_calls` stripped.
    let message = json!({
        "role": "assistant",
        "content": "diagnostic",
        "tool_calls": [{"type": "function", "function": {"name": "get_self_check"}}],
    });
    let out = filter_kernel_introspection_messages(&[message]);
    assert_eq!(out.len(), 1);
    assert!(out[0].get("tool_calls").is_none(), "out={out:?}");
}

#[test]
fn tool_value_arguments_name_the_tool_in_the_error() {
    let parsed: ReadFileArgs = parse_tool_value_arguments(
        json!({"path": "a.txt", "offset": 2, "limit": 3}),
        "read_file",
    )
    .expect("valid arguments");
    assert_eq!(parsed.path, "a.txt");
    assert_eq!(parsed.offset, Some(2));
    assert_eq!(parsed.limit, Some(3));

    let err = parse_tool_value_arguments::<ReadFileArgs>(json!({}), "read_file")
        .expect_err("missing path");
    assert!(
        err.to_string().starts_with(
            "LLM response invalid: shell agent tool read_file had invalid arguments: "
        ),
        "err={err}"
    );
}

/// A value whose `Serialize` always fails, so `to_json_value`'s error arm is
/// executable.
struct Unserialisable;

impl serde::Serialize for Unserialisable {
    fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("boom"))
    }
}

#[test]
fn to_json_value_reports_the_label_and_the_serializer_error() {
    assert_eq!(
        to_json_value("widget", 7).expect("integers serialise"),
        json!(7)
    );
    let err = to_json_value("widget", Unserialisable).expect_err("must fail");
    assert_eq!(
        err.to_string(),
        "internal invariant broken: failed to serialize widget: boom"
    );
}

#[test]
fn io_error_at_preserves_the_os_message_byte_for_byte() {
    let err = io_error_at(
        Path::new("/tmp/cordis-io-seam"),
        &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
    );
    assert_eq!(err.to_string(), "I/O at /tmp/cordis-io-seam: denied");
}

#[test]
fn invoke_payload_error_preserves_the_serializer_message() {
    let parse_error = serde_json::from_str::<Value>("{").expect_err("malformed json");
    let err = invoke_payload_error(parse_error);
    assert!(
        err.to_string().starts_with(
            "internal invariant broken: failed to serialize invoke payload for agent tool: "
        ),
        "err={err}"
    );
}

// ─────────────────────────── introspection tools ────────────────────────────

#[test]
#[serial]
fn runtime_status_reports_the_booted_workspace() {
    let (_temp, host) = empty_host();
    let value = AgentToolHost::agent_runtime_status(&host).expect("status");
    assert_eq!(
        value["fixtures_root"],
        host.fixtures_root().display().to_string()
    );
    // Even an empty index carries the Kernel's builtin `cordis` plugin and its
    // `agent_router` node.
    assert_eq!(value["plugin_count"], 1);
    assert_eq!(value["node_count"], 1);
    assert!(value["current_snapshot_id"].is_string());
}

#[test]
#[serial]
fn inject_turn_meta_forwards_to_the_host_and_passes_a_payload_without_meta_through() {
    let (_temp, host) = empty_host();
    let payload = json!({"value": 1});
    // No turn meta recorded for this session: the payload is returned as-is.
    assert_eq!(
        AgentToolHost::inject_turn_meta(&host, "no-meta-session", payload.clone()),
        payload
    );
}

#[test]
#[serial]
fn list_plugins_separates_loaded_from_unavailable_entries() {
    let (_temp, host) = host_with_unavailable_plugin();
    let value = AgentToolHost::agent_list_plugins(&host).expect("list");
    let plugins = value["plugins"].as_array().expect("plugins array");

    let demo = plugins
        .iter()
        .find(|plugin| plugin["plugin_path"] == "demo")
        .expect("demo listed");
    assert_eq!(demo["load_result"], "Loaded");
    assert_eq!(demo["required"], true);
    let node_ids: Vec<&str> = demo["node_ids"]
        .as_array()
        .expect("node ids")
        .iter()
        .map(|id| id.as_str().expect("node id string"))
        .collect();
    assert_eq!(node_ids, vec!["echo", "hidden"]);

    let plain = plugins
        .iter()
        .find(|plugin| plugin["plugin_path"] == "plain")
        .expect("plain listed");
    assert_eq!(plain["load_result"], "Unavailable(InitFailed)");
    assert_eq!(plain["node_ids"], json!([]), "no docs -> empty node list");

    let builtin = plugins
        .iter()
        .find(|plugin| plugin["plugin_path"] == "cordis")
        .expect("builtin listed");
    assert_eq!(builtin["load_result"], "Loaded");
    assert_eq!(builtin["node_ids"], json!(["agent_router"]));
}

#[test]
#[serial]
fn list_nodes_reports_every_registered_node() {
    let (_temp, host) = json_plugin_host();
    let value = AgentToolHost::agent_list_nodes(&host).expect("list");
    let nodes = value["nodes"].as_array().expect("nodes array");
    let fqns: Vec<&str> = nodes
        .iter()
        .map(|node| node["node_fqn"].as_str().expect("fqn"))
        .collect();
    assert_eq!(
        fqns,
        vec!["cordis::agent_router", "demo::echo", "demo::hidden"]
    );
    let demo_nodes: Vec<&Value> = nodes
        .iter()
        .filter(|node| node["plugin_path"] == "demo")
        .collect();
    assert_eq!(demo_nodes.len(), 2);
}

#[test]
#[serial]
fn kernel_status_and_issues_serialise() {
    let (_temp, host) = empty_host();
    let status = AgentToolHost::agent_kernel_status(&host).expect("status");
    assert_eq!(
        status["workspace_root"],
        host.fixtures_root().display().to_string()
    );
    let issues = AgentToolHost::agent_kernel_issues(&host).expect("issues");
    assert!(issues.is_array(), "issues={issues}");
}

#[test]
#[serial]
fn spill_writes_the_text_under_the_data_dir_and_sanitises_the_tool_name() {
    let (_temp, host) = empty_host();
    let path = AgentToolHost::agent_spill(&host, "web/search", "a long result").expect("spill");
    let path = PathBuf::from(path);
    assert!(
        path.starts_with(host.data_dir().join("spill")),
        "path={}",
        path.display()
    );
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.starts_with("web_search-"), "name={name}");
    assert_eq!(
        fs::read_to_string(&path).expect("read spill"),
        "a long result"
    );
}

#[test]
#[serial]
fn spill_reports_a_data_dir_that_is_a_file() {
    let (_temp, host) = empty_host();
    // `data` is a file, so `data/spill` cannot be created.
    fs::write(host.data_dir(), b"not a dir").expect("data file");
    let err = AgentToolHost::agent_spill(&host, "web", "text").expect_err("spill fails");
    let expected_path = host.data_dir().join("spill");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", expected_path.display())),
        "err={err}"
    );
}

#[test]
#[serial]
fn spill_reports_a_filename_longer_than_the_filesystem_allows() {
    let (_temp, host) = empty_host();
    // The sanitised tool name keeps its length, so a 300-char name makes the
    // filename exceed NAME_MAX and `fs::write` fails with ENAMETOOLONG.
    let tool = "x".repeat(300);
    let err = AgentToolHost::agent_spill(&host, &tool, "text").expect_err("name too long");
    assert!(err.to_string().starts_with("I/O at "), "err={err}");
}

#[test]
#[serial]
fn plugin_hints_render_the_catalogue_and_system_hints() {
    let (_temp, host) = host_with_unavailable_plugin();
    let hints = AgentToolHost::agent_plugin_hints(&host);
    assert_eq!(hints.len(), 2, "hints={hints:?}");
    assert!(
        hints[0].starts_with(crate::plugin::node_catalog::CATALOG_HEADER),
        "hints[0]={}",
        hints[0]
    );
    assert!(hints[0].contains("demo") && hints[0].contains("echo"));
    assert!(
        !hints[0].contains("hidden"),
        "inaccessible nodes are not advertised: {}",
        hints[0]
    );
    assert_eq!(hints[1], "demo hint");
}

#[test]
#[serial]
fn self_check_reports_open_signatures_instruction_and_lessons() {
    let (_temp, host) = empty_host();
    let sessions = host.data_dir().join("sessions");
    fs::create_dir_all(&sessions).expect("sessions dir");
    // Two empty assistant turns in one stored session — one `Empty` signature.
    fs::write(
        sessions.join("sess-1.json"),
        serde_json::to_string(&json!({
            "transcript": [
                {"kind": "assistant", "tools": [], "content": ""},
                {"kind": "assistant", "tools": [], "content": "   "},
            ]
        }))
        .expect("session json"),
    )
    .expect("session snapshot");
    fs::write(
        host.data_dir().join("selfcheck.json"),
        serde_json::to_string(&json!({
            "attempted": [],
            "lessons": [{
                "signature": "empty-abc",
                "symptom": "nothing came back",
                "fix": "retry the provider call",
                "outcome": "failed",
                "learned_at_ms": 1,
            }]
        }))
        .expect("ledger json"),
    )
    .expect("ledger");

    let value = AgentToolHost::agent_self_check(&host).expect("self check");
    assert_eq!(value["ok"], true);
    assert_eq!(value["scanned_sessions"], 1);
    assert_eq!(value["scanned_turns"], 2);
    let open = value["open"].as_array().expect("open array");
    assert_eq!(open.len(), 1, "value={value}");
    assert_eq!(open[0]["kind"], "empty");
    assert_eq!(open[0]["count"], 2);
    assert_eq!(open[0]["score"], 4);
    assert_eq!(open[0]["sessions"], json!(["sess-1"]));
    assert!(value["instruction"]
        .as_str()
        .expect("instruction")
        .contains("Failure kind: empty"));
    assert_eq!(value["lessons"].as_array().expect("lessons").len(), 1);
}

#[test]
#[serial]
fn self_check_on_an_empty_workspace_has_nothing_to_report() {
    let (_temp, host) = empty_host();
    let value = AgentToolHost::agent_self_check(&host).expect("self check");
    assert_eq!(value["scanned_sessions"], 0);
    assert_eq!(value["open"], json!([]));
    assert_eq!(value["instruction"], Value::Null);
    assert_eq!(value["lessons"], json!([]));
}

#[test]
#[serial]
fn self_check_digest_is_computed_once_and_then_cached() {
    let (_temp, host) = empty_host();
    let sessions = host.data_dir().join("sessions");
    fs::create_dir_all(&sessions).expect("sessions dir");
    // `digest` only reports signatures seen at least five times.
    let turns: Vec<Value> = (0..5)
        .map(|_| json!({"kind": "assistant", "tools": [], "content": ""}))
        .collect();
    fs::write(
        sessions.join("sess-digest.json"),
        serde_json::to_string(&json!({"transcript": turns})).expect("session json"),
    )
    .expect("session snapshot");

    let first = AgentToolHost::agent_self_check_digest(&host).expect("first digest");
    assert!(first.contains("empty × 5"), "digest={first}");
    // Second call hits the cache; the digest is the same because nothing
    // re-scanned the sessions directory.
    fs::write(
        sessions.join("sess-digest.json"),
        serde_json::to_string(&json!({"transcript": []})).expect("cleared json"),
    )
    .expect("clear snapshot");
    let second = AgentToolHost::agent_self_check_digest(&host).expect("cached digest");
    assert_eq!(first, second);
}

// ───────────────────────── invoke / execute tools ───────────────────────────

#[test]
#[serial]
fn invoke_plugin_resolves_the_plugin_from_the_node_when_the_path_is_empty() {
    let (_temp, host) = json_plugin_host();
    let session = start_session(&host);
    let value =
        AgentToolHost::agent_invoke_plugin(&host, &session, "", "echo", json!({"hello": "world"}))
            .expect("invoke");
    assert_eq!(value["plugin_path"], "demo");
    assert_eq!(value["node_id"], "echo");
    // The JSON artifact's process execution is `/bin/cat`, so the payload
    // round-trips through a real child process.
    assert_eq!(value["payload"], json!({"hello": "world"}));
}

#[test]
#[serial]
fn invoke_plugin_honours_an_explicit_plugin_path() {
    let (_temp, host) = json_plugin_host();
    let session = start_session(&host);
    let value =
        AgentToolHost::agent_invoke_plugin(&host, &session, "/demo", "echo", json!({"n": 1}))
            .expect("invoke");
    assert_eq!(value["plugin_path"], "/demo");
    assert_eq!(value["payload"], json!({"n": 1}));
}

#[test]
#[serial]
fn invoke_plugin_refuses_a_node_the_agent_may_not_call() {
    let (_temp, host) = json_plugin_host();
    let session = start_session(&host);
    let err = AgentToolHost::agent_invoke_plugin(&host, &session, "demo", "hidden", json!({}))
        .expect_err("hidden node");
    assert_eq!(
        err.to_string(),
        "invalid argument: Agent is not allowed to call demo::hidden"
    );
}

#[test]
#[serial]
fn invoke_plugin_cannot_resolve_a_node_no_plugin_declares() {
    let (_temp, host) = json_plugin_host();
    let session = start_session(&host);
    let err = AgentToolHost::agent_invoke_plugin(&host, &session, "", "no_such_node", json!({}))
        .expect_err("unknown node");
    assert_eq!(
        err.to_string(),
        "invalid argument: invoke_plugin needs a plugin_path: no loaded plugin declares node \
         no_such_node"
    );
}

#[test]
#[serial]
fn execute_target_runs_a_registered_node() {
    let (_temp, host) = json_plugin_host();
    let value =
        AgentToolHost::agent_execute_target(&host, "demo::echo", json!({"value": 2})).expect("run");
    assert_eq!(value["target_node_fqn"], "demo::echo");
    assert!(value["output"]["execution_id"].is_string(), "value={value}");
}

#[test]
#[serial]
fn execute_target_rejects_an_inaccessible_node_and_an_unknown_fqn() {
    let (_temp, host) = json_plugin_host();
    let err = AgentToolHost::agent_execute_target(&host, "demo::hidden", json!({}))
        .expect_err("hidden node");
    assert_eq!(
        err.to_string(),
        "invalid argument: Agent is not allowed to call demo::hidden"
    );

    // No `::`: the accessibility gate is skipped and the executor reports the
    // missing registered node.
    let err = AgentToolHost::agent_execute_target(&host, "not_a_node", json!({}))
        .expect_err("unknown fqn");
    assert_eq!(
        err.to_string(),
        "invalid argument: registered node not found: not_a_node"
    );
}

// ─────────────────────────────── file tools ─────────────────────────────────

fn write_text(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent dir");
    }
    fs::write(path, text).expect("write text");
}

#[test]
#[serial]
fn read_file_slices_by_offset_and_limit() {
    let (_temp, host) = empty_host();
    write_text(
        &host.fixtures_root().join("src/main.rs"),
        "one\ntwo\nthree\n",
    );

    let value = AgentToolHost::agent_read_file(&host, "src/main.rs", None, None).expect("read");
    assert_eq!(value["path"], "src/main.rs");
    assert_eq!(value["total_lines"], 3);
    assert_eq!(value["offset"], 0);
    assert_eq!(value["limit"], 3);
    assert_eq!(
        value["lines"],
        json!([
            {"line": 1, "text": "one"},
            {"line": 2, "text": "two"},
            {"line": 3, "text": "three"},
        ])
    );

    let value =
        AgentToolHost::agent_read_file(&host, "src/main.rs", Some(1), Some(1)).expect("slice");
    assert_eq!(value["offset"], 1);
    assert_eq!(value["limit"], 1);
    assert_eq!(value["lines"], json!([{"line": 2, "text": "two"}]));

    // Offset past the end clamps to the last line; a limit past the end only
    // reports the lines that exist.
    let value =
        AgentToolHost::agent_read_file(&host, "src/main.rs", Some(99), None).expect("past end");
    assert_eq!(value["offset"], 3);
    assert_eq!(value["limit"], 0);
    assert_eq!(value["lines"], json!([]));
    let value =
        AgentToolHost::agent_read_file(&host, "src/main.rs", Some(1), Some(99)).expect("big limit");
    assert_eq!(value["limit"], 2);
}

#[test]
#[serial]
fn read_file_rejects_sensitive_paths_and_oversized_files() {
    let (_temp, host) = empty_host();
    let err = AgentToolHost::agent_read_file(&host, "etc/passwd", None, None)
        .expect_err("sensitive path");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (/etc/passwd)"
    );

    // 17 MiB: over the 16 MiB read cap.
    let big = host.fixtures_root().join("big.bin");
    let file = fs::File::create(&big).expect("create big file");
    file.set_len(17 * 1024 * 1024).expect("sparse big file");
    let err = AgentToolHost::agent_read_file(&host, "big.bin", None, None).expect_err("too big");
    assert!(
        err.to_string()
            .starts_with("invalid argument: read_file: file big.bin exceeds 16 MiB limit (size="),
        "err={err}"
    );
}

#[test]
#[serial]
fn read_file_reports_io_errors() {
    let (_temp, host) = empty_host();
    fs::create_dir_all(host.fixtures_root().join("src")).expect("src dir");
    let err = AgentToolHost::agent_read_file(&host, "src", None, None).expect_err("directory");
    assert!(
        err.to_string().starts_with(&format!(
            "I/O at {}: ",
            host.fixtures_root().join("src").display()
        )),
        "err={err}"
    );

    // A missing file fails the metadata pre-check (the size cap is skipped) and
    // then the read itself.
    let err =
        AgentToolHost::agent_read_file(&host, "missing.txt", None, None).expect_err("missing");
    assert_eq!(
        err.to_string(),
        format!(
            "I/O at {}: No such file or directory (os error 2)",
            host.fixtures_root().join("missing.txt").display()
        )
    );
}

#[test]
#[serial]
fn list_directory_sorts_directories_before_files() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    fs::create_dir_all(root.join("tree/sub")).expect("sub dir");
    write_text(&root.join("tree/b.txt"), "b");
    write_text(&root.join("tree/a.txt"), "a");

    let value = AgentToolHost::agent_list_directory(&host, "tree").expect("list");
    assert_eq!(
        value["entries"],
        json!([
            {"name": "sub", "kind": "dir"},
            {"name": "a.txt", "kind": "file"},
            {"name": "b.txt", "kind": "file"},
        ])
    );
}

#[test]
#[serial]
fn list_directory_of_a_file_or_a_missing_path_is_empty() {
    let (_temp, host) = empty_host();
    write_text(&host.fixtures_root().join("file.txt"), "x");
    for path in ["file.txt", "does/not/exist"] {
        let value = AgentToolHost::agent_list_directory(&host, path).expect("list");
        assert_eq!(value["path"], path);
        assert_eq!(value["entries"], json!([]), "path={path}");
    }

    let err = AgentToolHost::agent_list_directory(&host, ".ssh").expect_err("sensitive");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (.ssh)"
    );
}

#[test]
#[serial]
fn search_code_finds_matches_and_skips_undecodable_files() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    write_text(&root.join("src/a.rs"), "needle one\nplain\nneedle two\n");
    // A source-looking file that is not UTF-8: the walker must skip it rather
    // than abort the search.
    fs::write(root.join("src/broken.rs"), [0xff, 0xfe, 0x00]).expect("binary source file");

    let value = AgentToolHost::agent_search_code(&host, "needle", Some("src")).expect("search");
    assert_eq!(value["pattern"], "needle");
    assert_eq!(value["search_root"], "src");
    assert_eq!(value["files_walked"], 2);
    assert_eq!(
        value["matches"],
        json!([
            {"path": "a.rs", "line": 1, "text": "needle one"},
            {"path": "a.rs", "line": 3, "text": "needle two"},
        ])
    );

    // No path: the search root is the fixtures tree, so the relative display
    // is empty.
    let value = AgentToolHost::agent_search_code(&host, "needle", None).expect("search root");
    assert_eq!(value["search_root"], "");
    assert_eq!(value["matches"].as_array().expect("matches").len(), 2);
}

#[test]
#[serial]
fn search_code_stops_after_forty_hits() {
    let (_temp, host) = empty_host();
    let hits: String = (0..45).map(|i| format!("needle {i}\n")).collect();
    write_text(&host.fixtures_root().join("src/hits.rs"), &hits);

    let value = AgentToolHost::agent_search_code(&host, "needle", Some("src")).expect("search");
    assert_eq!(value["matches"].as_array().expect("matches").len(), 40);
    // The walk stops as soon as the cap is reached.
    assert_eq!(value["files_walked"], 1);
}

#[test]
#[serial]
fn search_code_reports_an_outside_root_with_its_absolute_path() {
    let (_temp, host) = empty_host();
    // `data/` resolves against the workspace root (the fixture root's parent),
    // which is outside `fixtures/`, so `strip_prefix` cannot shorten it.
    let value = AgentToolHost::agent_search_code(&host, "needle", Some("data")).expect("search");
    let expected = host
        .fixtures_root()
        .parent()
        .expect("workspace root")
        .join("data");
    assert_eq!(value["search_root"], expected.display().to_string());
    assert_eq!(value["files_walked"], 0);

    let err = AgentToolHost::agent_search_code(&host, "needle", Some(".ssh")).expect_err("blocked");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (.ssh)"
    );
}

#[test]
#[serial]
fn write_file_creates_parents_and_records_the_written_size() {
    let (_temp, host) = empty_host();
    let value = AgentToolHost::agent_write_file(&host, "plugins/new/src/lib.rs", "fn main() {}\n")
        .expect("write");
    assert_eq!(value["path"], "plugins/new/src/lib.rs");
    assert_eq!(value["written_bytes"], 13);
    assert_eq!(
        fs::read_to_string(host.fixtures_root().join("plugins/new/src/lib.rs")).expect("read back"),
        "fn main() {}\n"
    );

    // Overwriting an existing file keeps the previous bytes as the rollback
    // backup and reports the new length.
    let value =
        AgentToolHost::agent_write_file(&host, "plugins/new/src/lib.rs", "x").expect("write");
    assert_eq!(value["written_bytes"], 1);
}

#[test]
#[serial]
fn write_file_reports_a_parent_that_is_a_file_and_a_directory_target() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    write_text(&root.join("blocker"), "not a dir");

    let err = AgentToolHost::agent_write_file(&host, "blocker/x.txt", "x").expect_err("parent");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", root.join("blocker").display())),
        "err={err}"
    );

    // The target itself is a directory: create_dir_all succeeds, then the
    // write fails.
    fs::create_dir_all(root.join("src")).expect("src dir");
    let err = AgentToolHost::agent_write_file(&host, "src", "x").expect_err("dir target");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", root.join("src").display())),
        "err={err}"
    );

    let err = AgentToolHost::agent_write_file(&host, "credentials", "x").expect_err("sensitive");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (credentials)"
    );
}

#[test]
#[serial]
fn replace_in_file_requires_a_unique_match() {
    let (_temp, host) = empty_host();
    let path = host.fixtures_root().join("src/t.txt");
    write_text(&path, "alpha beta gamma\n");

    let value =
        AgentToolHost::agent_replace_in_file(&host, "src/t.txt", "beta", "B").expect("replace");
    assert_eq!(value["replaced"], true);
    assert_eq!(value["occurrences"], 1);
    assert_eq!(fs::read_to_string(&path).expect("read"), "alpha B gamma\n");

    let err = AgentToolHost::agent_replace_in_file(&host, "src/t.txt", "zzz", "B")
        .expect_err("not found");
    assert_eq!(
        err.to_string(),
        "invalid argument: replace_in_file: pattern not found in src/t.txt: zzz"
    );

    write_text(&path, "dup dup\n");
    let err = AgentToolHost::agent_replace_in_file(&host, "src/t.txt", "dup", "one")
        .expect_err("ambiguous");
    assert_eq!(
        err.to_string(),
        "invalid argument: replace_in_file: pattern appears 2 times in src/t.txt; provide more \
         surrounding context so the target is unique"
    );

    fs::create_dir_all(host.fixtures_root().join("adir")).expect("adir");
    let err =
        AgentToolHost::agent_replace_in_file(&host, "adir", "x", "y").expect_err("read fails");
    assert!(
        err.to_string().starts_with(&format!(
            "I/O at {}: ",
            host.fixtures_root().join("adir").display()
        )),
        "err={err}"
    );
}

#[test]
#[serial]
fn append_file_appends_to_new_and_existing_files() {
    let (_temp, host) = empty_host();
    let value = AgentToolHost::agent_append_file(&host, "logs/a.txt", "first").expect("append");
    assert_eq!(value["appended_bytes"], 5);
    let path = host.fixtures_root().join("logs/a.txt");
    assert_eq!(fs::read_to_string(&path).expect("read"), "first");

    let value = AgentToolHost::agent_append_file(&host, "logs/a.txt", "-second").expect("append");
    assert_eq!(value["appended_bytes"], 7);
    assert_eq!(fs::read_to_string(&path).expect("read"), "first-second");
}

#[test]
#[serial]
fn append_file_reports_open_and_parent_failures() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    fs::create_dir_all(root.join("adir")).expect("adir");
    let err = AgentToolHost::agent_append_file(&host, "adir", "x").expect_err("open a dir");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", root.join("adir").display())),
        "err={err}"
    );

    write_text(&root.join("blocker"), "not a dir");
    let err = AgentToolHost::agent_append_file(&host, "blocker/x", "x").expect_err("parent");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", root.join("blocker").display())),
        "err={err}"
    );

    let err = AgentToolHost::agent_append_file(&host, "auth.json", "x").expect_err("sensitive");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (auth.json)"
    );
}

#[test]
#[serial]
fn delete_file_removes_a_file_and_refuses_a_directory() {
    let (_temp, host) = empty_host();
    let path = host.fixtures_root().join("gone.txt");
    write_text(&path, "bye");
    let value = AgentToolHost::agent_delete_file(&host, "gone.txt").expect("delete");
    assert_eq!(value["deleted"], true);
    assert!(!path.exists());

    fs::create_dir_all(host.fixtures_root().join("adir")).expect("adir");
    let err = AgentToolHost::agent_delete_file(&host, "adir").expect_err("directory");
    assert_eq!(
        err.to_string(),
        "invalid argument: delete_file: path is a directory, not a file: adir"
    );

    let err = AgentToolHost::agent_delete_file(&host, "missing.txt").expect_err("missing");
    assert!(err.to_string().starts_with("I/O at "), "err={err}");

    let err = AgentToolHost::agent_delete_file(&host, ".env").expect_err("sensitive");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (.env)"
    );
}

#[test]
#[serial]
fn rename_file_moves_files_and_backs_up_the_destination() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    write_text(&root.join("a.txt"), "from a");
    let value = AgentToolHost::agent_rename_file(&host, "a.txt", "nested/b.txt").expect("rename");
    assert_eq!(value["renamed"], true);
    assert_eq!(value["new_path"], "nested/b.txt");
    assert!(!root.join("a.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("nested/b.txt")).expect("read"),
        "from a"
    );

    // Renaming onto an existing file backs that file up too, so revert can
    // restore both sides.
    write_text(&root.join("src1.txt"), "one");
    write_text(&root.join("dst1.txt"), "two");
    AgentToolHost::agent_rename_file(&host, "src1.txt", "dst1.txt").expect("overwrite");
    assert_eq!(
        fs::read_to_string(root.join("dst1.txt")).expect("read"),
        "one"
    );
}

#[test]
#[serial]
fn rename_file_reports_failures() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    let err = AgentToolHost::agent_rename_file(&host, "missing.txt", "dst.txt").expect_err("src");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", root.join("missing.txt").display())),
        "err={err}"
    );

    write_text(&root.join("src.txt"), "x");
    write_text(&root.join("blocker"), "not a dir");
    let err =
        AgentToolHost::agent_rename_file(&host, "src.txt", "blocker/dst.txt").expect_err("parent");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", root.join("blocker").display())),
        "err={err}"
    );

    let err = AgentToolHost::agent_rename_file(&host, "src.txt", ".ssh/id_rsa")
        .expect_err("sensitive dst");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (.ssh)"
    );
    let err = AgentToolHost::agent_rename_file(&host, ".ssh/id_rsa", "out.txt")
        .expect_err("sensitive src");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (.ssh)"
    );
}

#[test]
#[serial]
fn move_file_is_rename_file() {
    let (_temp, host) = empty_host();
    write_text(&host.fixtures_root().join("m.txt"), "m");
    let value = AgentToolHost::agent_move_file(&host, "m.txt", "moved.txt").expect("move");
    assert_eq!(value["renamed"], true);
    assert_eq!(value["path"], "m.txt");
    assert_eq!(value["new_path"], "moved.txt");
}

#[test]
#[serial]
fn copy_file_copies_and_backs_up_an_existing_destination() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    write_text(&root.join("src.txt"), "content");
    let value = AgentToolHost::agent_copy_file(&host, "src.txt", "nested/copy.txt").expect("copy");
    assert_eq!(value["copied"], true);
    assert_eq!(
        fs::read_to_string(root.join("nested/copy.txt")).expect("read"),
        "content"
    );
    assert_eq!(
        fs::read_to_string(root.join("src.txt")).expect("read"),
        "content"
    );

    write_text(&root.join("dst.txt"), "old");
    AgentToolHost::agent_copy_file(&host, "src.txt", "dst.txt").expect("overwrite");
    assert_eq!(
        fs::read_to_string(root.join("dst.txt")).expect("read"),
        "content"
    );
}

#[test]
#[serial]
fn copy_file_reports_failures() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    let err = AgentToolHost::agent_copy_file(&host, "missing.txt", "dst.txt").expect_err("src");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", root.join("missing.txt").display())),
        "err={err}"
    );

    write_text(&root.join("src.txt"), "x");
    write_text(&root.join("blocker"), "not a dir");
    let err =
        AgentToolHost::agent_copy_file(&host, "src.txt", "blocker/dst.txt").expect_err("parent");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", root.join("blocker").display())),
        "err={err}"
    );

    let err =
        AgentToolHost::agent_copy_file(&host, ".ssh/id_rsa", "out").expect_err("sensitive src");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (.ssh)"
    );
    let err =
        AgentToolHost::agent_copy_file(&host, "src.txt", "auth.json").expect_err("sensitive dst");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: path references sensitive resource (auth.json)"
    );
}

#[test]
#[serial]
fn revert_changes_restores_every_edit_and_then_reports_zero() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    AgentToolHost::agent_write_file(&host, "created.txt", "new").expect("create");
    write_text(&root.join("original.txt"), "before");
    AgentToolHost::agent_write_file(&host, "original.txt", "after").expect("overwrite");
    write_text(&root.join("removed.txt"), "bye");
    AgentToolHost::agent_delete_file(&host, "removed.txt").expect("delete");

    let value = AgentToolHost::agent_revert_changes(&host).expect("revert");
    assert_eq!(value["reverted_files"], 3);
    assert!(!root.join("created.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("original.txt")).expect("read"),
        "before"
    );
    assert_eq!(
        fs::read_to_string(root.join("removed.txt")).expect("read"),
        "bye"
    );

    let value = AgentToolHost::agent_revert_changes(&host).expect("revert again");
    assert_eq!(value["reverted_files"], 0);
}

// ─────────────────────────────── run tools ─────────────────────────────────

#[test]
#[serial]
fn run_command_dispatches_an_argv_without_a_shell() {
    let (_temp, host) = empty_host();
    let value = AgentToolHost::agent_run_command(&host, "/bin/echo hello").expect("run");
    assert_eq!(value["stdout"], "hello\n");
    assert_eq!(value["stderr"], "");
    assert_eq!(value["exit_code"], 0);

    let err = AgentToolHost::agent_run_command(&host, "ssh host").expect_err("sensitive");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: command references sensitive operation (ssh)"
    );
}

#[test]
#[serial]
fn run_command_reports_tokenisation_and_spawn_failures() {
    let (_temp, host) = empty_host();
    let err = AgentToolHost::agent_run_command(&host, "unclosed 'quote").expect_err("quote");
    assert!(
        err.to_string()
            .starts_with("invalid argument: run_command tokenisation failed: "),
        "err={err}"
    );

    let err = AgentToolHost::agent_run_command(&host, "").expect_err("empty");
    assert_eq!(
        err.to_string(),
        "invalid argument: run_command received an empty command string"
    );

    let err = AgentToolHost::agent_run_command(&host, "'' arg").expect_err("empty program");
    assert_eq!(
        err.to_string(),
        "invalid argument: run_command program was empty after tokenisation"
    );

    let err = AgentToolHost::agent_run_command(&host, "/definitely-not-a-real-binary-xyz")
        .expect_err("spawn failure");
    assert!(
        err.to_string().starts_with(&format!(
            "I/O at {}: failed to run command: ",
            host.fixtures_root().display()
        )),
        "err={err}"
    );
}

struct PathGuard {
    previous: Option<std::ffi::OsString>,
}

impl PathGuard {
    /// Put `prefix` in front of the inherited `PATH`.
    fn prepend(prefix: &Path) -> Self {
        let previous = std::env::var_os("PATH");
        let mut value = prefix.as_os_str().to_owned();
        if let Some(previous) = previous.as_ref() {
            value.push(":");
            value.push(previous);
        }
        std::env::set_var("PATH", value);
        Self { previous }
    }

    /// Replace `PATH` entirely, so nothing outside `prefix` is findable.
    fn only(prefix: &Path) -> Self {
        let previous = std::env::var_os("PATH");
        std::env::set_var("PATH", prefix);
        Self { previous }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => std::env::set_var("PATH", previous),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// Install a fake `cargo` in front of `PATH` that runs `body` as a shell
/// script. Returns the guard whose lifetime owns the directory and restores
/// `PATH` on drop.
fn fake_cargo(body: &str) -> (TempDir, PathGuard) {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().expect("cargo bin dir");
    let path = dir.path().join("cargo");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake cargo");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake cargo");
    let guard = PathGuard::prepend(dir.path());
    (dir, guard)
}

#[test]
#[serial]
fn build_plugins_reports_a_missing_cargo() {
    let (_temp, host) = empty_host();
    let empty_bin = TempDir::new().expect("empty bin");
    let _guard = PathGuard::only(empty_bin.path());
    let err = AgentToolHost::agent_build_plugins(&host, "all").expect_err("cargo missing");
    assert!(
        err.to_string()
            .starts_with("invalid argument: cargo build failed to start: "),
        "err={err}"
    );
}

#[test]
#[serial]
fn build_plugins_reports_a_failing_cargo_without_touching_artifacts() {
    let (_temp, host) = empty_host();
    let (_bin, _guard) = fake_cargo("printf 'fake stdout\\n'; printf 'fake stderr\\n' >&2; exit 3");
    let value = AgentToolHost::agent_build_plugins(&host, "demo").expect("cargo ran");
    assert_eq!(value["ok"], false);
    assert_eq!(value["exit_code"], 3);
    assert_eq!(value["stdout"], "fake stdout\n");
    assert_eq!(value["stderr"], "fake stderr\n");
    assert!(
        value.get("synced_artifact").is_none(),
        "a failed build must not sync: {value}"
    );
}

#[test]
#[serial]
fn build_plugins_syncs_the_artifact_and_reloads() {
    let (_temp, host) = empty_host();
    let fixtures = host.fixtures_root().to_path_buf();
    let body = format!(
        "mkdir -p '{0}/plugins/target/debug'\nprintf 'fake-so-bytes' > '{0}/plugins/target/debug/libfresh.so'\nexit 0",
        fixtures.display()
    );
    let (_bin, _guard) = fake_cargo(&body);

    let value = AgentToolHost::agent_build_plugins(&host, "fresh").expect("build");
    assert_eq!(value["ok"], true);
    assert_eq!(value["exit_code"], 0);
    let synced = value["synced_artifact"].as_str().expect("synced artifact");
    assert!(
        synced.ends_with("artifacts/fresh.so (13 bytes)"),
        "synced={synced}"
    );
    assert_eq!(
        fs::read(fixtures.join("artifacts/fresh.so")).expect("synced file"),
        b"fake-so-bytes"
    );
    // `fresh` is not in the running snapshot nor in the index, so the reload is
    // the documented no-op that still reports `reloaded`.
    assert_eq!(value["reload"]["status"], "reloaded");
}

#[test]
#[serial]
fn build_plugins_skips_the_sync_when_the_artifact_is_missing() {
    let (_temp, host) = empty_host();
    let (_bin, _guard) = fake_cargo("exit 0");
    let value = AgentToolHost::agent_build_plugins(&host, "fresh").expect("build");
    assert_eq!(value["ok"], true);
    assert_eq!(value["synced_artifact"], Value::Null);
    assert!(
        value.get("reload").is_none(),
        "nothing synced -> nothing reloaded"
    );
}

#[test]
#[serial]
fn build_plugins_surfaces_a_failed_reload_after_sync() {
    let (_temp, host) = json_plugin_host();
    let fixtures = host.fixtures_root().to_path_buf();
    let body = format!(
        "mkdir -p '{0}/plugins/target/debug'\nprintf 'not-a-dylib' > '{0}/plugins/target/debug/libdemo.so'\nexit 0",
        fixtures.display()
    );
    let (_bin, _guard) = fake_cargo(&body);

    // `demo` is in the running snapshot, and reloading it tries to open the
    // JSON artifact as a dynamic library: the attempt report carries the
    // failure rather than aborting the build result.
    let value = AgentToolHost::agent_build_plugins(&host, "demo").expect("build result");
    assert_eq!(value["reload"]["status"], "failed");
    assert!(
        value["reload"]["failure_summary"]
            .as_str()
            .expect("failure summary")
            .contains("invalid ELF header"),
        "value={value}"
    );
    assert!(
        fixtures.join("artifacts/demo.so").exists(),
        "the artifact was synced before the reload failed"
    );
}

#[test]
#[serial]
fn reload_runtime_returns_the_attempt_report() {
    let (_temp, host) = json_plugin_host();
    let value = AgentToolHost::agent_reload_runtime(&host, "/").expect("reload");
    assert_eq!(value["status"], "reloaded");
    assert!(value["snapshot_root"].is_string());
    // A subtree with no matching plugin and no index entry is a no-op reload.
    let value = AgentToolHost::agent_reload_runtime(&host, "/not-loaded").expect("no-op reload");
    assert_eq!(value["status"], "reloaded");
}

#[test]
#[serial]
fn run_plugin_test_uses_the_supplied_or_default_command() {
    let (_temp, host) = empty_host();
    let value = AgentToolHost::agent_run_plugin_test(&host, Some("/bin/echo hi")).expect("run");
    assert_eq!(value["command"], "/bin/echo hi");
    assert_eq!(value["success"], true);
    assert_eq!(value["exit_code"], 0);
    assert_eq!(value["stdout"], "hi\n");

    let (_bin, _guard) = fake_cargo("printf 'tests passed\\n'; exit 0");
    let value = AgentToolHost::agent_run_plugin_test(&host, None).expect("default command");
    assert_eq!(
        value["command"],
        "cargo test --quiet --manifest-path plugins/Cargo.toml"
    );
    assert_eq!(value["success"], true);
    assert_eq!(value["stdout"], "tests passed\n");
}

#[test]
#[serial]
fn run_plugin_test_reports_bad_commands() {
    let (_temp, host) = empty_host();
    let err =
        AgentToolHost::agent_run_plugin_test(&host, Some("unclosed 'quote")).expect_err("quote");
    assert!(
        err.to_string()
            .starts_with("invalid argument: run_plugin_test tokenisation failed: "),
        "err={err}"
    );

    let err = AgentToolHost::agent_run_plugin_test(&host, Some("")).expect_err("empty");
    assert_eq!(
        err.to_string(),
        "invalid argument: run_plugin_test received an empty command string"
    );

    let err = AgentToolHost::agent_run_plugin_test(&host, Some("'' arg")).expect_err("program");
    assert_eq!(
        err.to_string(),
        "invalid argument: run_plugin_test program was empty after tokenisation"
    );

    let err =
        AgentToolHost::agent_run_plugin_test(&host, Some("/definitely-not-a-real-binary-xyz"))
            .expect_err("spawn");
    assert!(
        err.to_string()
            .starts_with("command failed: /definitely-not-a-real-binary-xyz []: "),
        "err={err}"
    );

    let err = AgentToolHost::agent_run_plugin_test(&host, Some("ssh host")).expect_err("sensitive");
    assert_eq!(
        err.to_string(),
        "invalid argument: blocked: command references sensitive operation (ssh)"
    );
}

// ───────────────────────────── session tools ───────────────────────────────

#[test]
#[serial]
fn compact_context_compacts_a_live_session_and_defers_an_absent_one() {
    let (_temp, host) = empty_host();
    let session = start_session(&host);
    let value = AgentToolHost::agent_compact_context(&host, &session).expect("compact");
    assert_eq!(value["compacted"], true);
    assert_eq!(value["old_messages"], 0);
    assert_eq!(value["new_messages"], 0);

    // The session is not in the map (it is mid-turn from the host's point of
    // view), so the action is queued instead.
    let value = AgentToolHost::agent_compact_context(&host, "session-mid-turn").expect("defer");
    assert_eq!(value["compacted"], false);
    assert_eq!(value["deferred"], true);
    assert_eq!(
        value["reason"],
        "session is currently mid-turn; compact will run after this turn completes"
    );
}

#[test]
#[serial]
fn set_soul_stores_persona_and_profile_and_overlay_reads_it_back() {
    let (_temp, host) = empty_host();
    let value =
        AgentToolHost::agent_set_soul(&host, "user:1", Some("be concise"), None).expect("persona");
    assert_eq!(value["ok"], true);
    assert_eq!(value["soul_key"], "user:1");
    assert_eq!(value["persona_chars"], 10);
    assert_eq!(value["profile"], "default");
    assert_eq!(
        AgentToolHost::agent_soul_overlay(&host, "user:1").as_deref(),
        Some("be concise")
    );

    // A blank profile clears it back to `default`; a known profile sticks.
    let value =
        AgentToolHost::agent_set_soul(&host, "user:1", None, Some("  ")).expect("blank profile");
    assert_eq!(value["profile"], "default");
    let value =
        AgentToolHost::agent_set_soul(&host, "user:1", None, Some("default")).expect("profile");
    assert_eq!(value["profile"], "default");
    assert_eq!(
        AgentToolHost::agent_soul_overlay(&host, "user:1").as_deref(),
        Some("be concise"),
        "setting only the profile must not wipe the persona"
    );
}

#[test]
#[serial]
fn set_soul_rejects_an_empty_key_an_unknown_profile_and_a_broken_store() {
    let (_temp, host) = empty_host();
    let err = AgentToolHost::agent_set_soul(&host, "", Some("x"), None).expect_err("empty key");
    assert_eq!(
        err.to_string(),
        "invalid argument: this session has no user identity; soul editing requires a channel \
         session (Feishu/QQ) with sender info"
    );

    let err = AgentToolHost::agent_set_soul(&host, "user:1", None, Some("nope"))
        .expect_err("unknown profile");
    assert_eq!(
        err.to_string(),
        "invalid argument: unknown LLM profile 'nope'; available: default"
    );

    // A soul file that is a directory: `get_soul` reports an I/O error.
    let souls = host.data_dir().join("souls");
    fs::create_dir_all(souls.join("broken.json")).expect("soul dir standing in for a file");
    let err = AgentToolHost::agent_set_soul(&host, "broken", Some("x"), None)
        .expect_err("unreadable soul");
    assert!(err.to_string().starts_with("I/O at "), "err={err}");
    assert_eq!(
        AgentToolHost::agent_soul_overlay(&host, "broken"),
        None,
        "an unreadable soul yields no overlay"
    );
    assert_eq!(AgentToolHost::agent_soul_overlay(&host, ""), None);
}

#[test]
#[serial]
fn set_soul_reports_a_store_that_cannot_be_created() {
    let (_temp, host) = empty_host();
    let souls = host.data_dir().join("souls");
    fs::create_dir_all(host.data_dir()).expect("data dir");
    // Dangling symlink: `get_soul` sees NotFound (so the record starts empty),
    // while `create_dir_all` cannot create the target.
    #[cfg(unix)]
    std::os::unix::fs::symlink("/nonexistent-cordis-souls", &souls).expect("soul symlink");
    let err = AgentToolHost::agent_set_soul(&host, "user:1", Some("x"), None)
        .expect_err("cannot create store");
    assert!(
        err.to_string()
            .starts_with(&format!("I/O at {}: ", souls.display())),
        "err={err}"
    );
}

#[test]
#[serial]
fn create_plugin_scaffolds_a_workspace_member() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    write_text(
        &root.join("plugins/Cargo.toml"),
        "[workspace]\nmembers = []\nresolver = \"2\"\n",
    );
    let value = AgentToolHost::agent_create_plugin(&host, "fresh_plug", Some("a demo plugin"))
        .expect("create");
    assert_eq!(value["ok"], true);
    assert_eq!(value["plugin_path"], "/fresh_plug");
    assert!(
        value["message"]
            .as_str()
            .expect("message")
            .starts_with("Created plugin /fresh_plug with skeleton."),
        "value={value}"
    );
    assert!(root.join("plugins/fresh_plug/src/lib.rs").exists());
    assert!(root.join("plugins/fresh_plug/tests/fresh_plug.rs").exists());
    assert!(root
        .join("plugins/fresh_plug/docs/human/overview.md")
        .exists());
    let manifest = fs::read_to_string(root.join("plugins/Cargo.toml")).expect("manifest");
    assert!(manifest.contains("fresh_plug"), "manifest={manifest}");
}

#[test]
#[serial]
fn create_plugin_rejects_bad_names_and_reports_scaffold_failures() {
    let (_temp, host) = empty_host();
    let root = host.fixtures_root();
    let err = AgentToolHost::agent_create_plugin(&host, "", None).expect_err("empty");
    assert_eq!(
        err.to_string(),
        "invalid argument: plugin name must not be empty"
    );
    let err = AgentToolHost::agent_create_plugin(&host, "bad-name", None).expect_err("chars");
    assert_eq!(
        err.to_string(),
        "invalid argument: plugin name must contain only [a-zA-Z0-9_]"
    );
    fs::create_dir_all(root.join("plugins/taken")).expect("existing plugin");
    let err = AgentToolHost::agent_create_plugin(&host, "taken", None).expect_err("exists");
    assert_eq!(
        err.to_string(),
        "invalid argument: plugin directory already exists: plugins/taken"
    );

    // Scaffolding reaches the workspace manifest after writing the plugin
    // files; without one it reports the read failure.
    let err = AgentToolHost::agent_create_plugin(&host, "orphan", None).expect_err("no manifest");
    assert!(
        err.to_string().starts_with(&format!(
            "I/O at {}: failed to read workspace manifest: ",
            root.join("plugins/Cargo.toml").display()
        )),
        "err={err}"
    );
}

#[test]
#[serial]
fn send_warning_fans_out_through_the_notification_bus() {
    let (_temp, host) = empty_host();
    // No registered handlers: the call is a no-op that must not panic.
    AgentToolHost::agent_send_warning_to_test_groups(&host, "heads up");
}

// ─────────────────────────── plugin iteration ──────────────────────────────

/// A runner installed for the whole test binary. `iterate` mirrors the canned
/// result the host's own session tests expect (`iter-fake`, `Blocked`) so
/// whichever test installs first, the other module's assertions still hold.
struct CannedIterationRunner;

fn canned_result(
    iteration_id: &str,
    final_verdict: crate::kernel::plugin_iteration::PluginIterationFinalVerdict,
) -> crate::host::KernelPluginIterationResult {
    crate::host::KernelPluginIterationResult {
        iteration_id: iteration_id.to_string(),
        issue_id: "issue-canned".to_string(),
        root_plugin_path: "demo".to_string(),
        target_plugin_paths: Vec::new(),
        source: None,
        summary: "canned iteration".to_string(),
        agent_session_id: None,
        tool_execution_summary: None,
        derived_edit_plan: crate::kernel::plugin_iteration::PluginEditPlan {
            issue_id: "issue-canned".to_string(),
            patch_id: format!("{iteration_id}-empty"),
            summary: "canned iteration".to_string(),
            operations: Vec::new(),
        },
        transcript_excerpt: Vec::new(),
        changed_paths: Vec::new(),
        rebuilt_artifacts: Vec::new(),
        candidate: None,
        verification: None,
        verifier_verdict: None,
        canary: None,
        final_verdict,
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

impl crate::iteration_api::IterationRunner for CannedIterationRunner {
    fn iterate(
        &self,
        _host: &RuntimeHost,
        request: crate::kernel::plugin_iteration::KernelPluginIterationRequest,
        _origin_session_id: Option<&str>,
    ) -> Result<crate::host::KernelPluginIterationResult, RuntimeError> {
        if request.instruction.as_deref() == Some("fail") {
            return Err(RuntimeError::InvalidArgument {
                message: "runner refused".to_string(),
            });
        }
        Ok(canned_result(
            "iter-fake",
            crate::kernel::plugin_iteration::PluginIterationFinalVerdict::Blocked,
        ))
    }

    fn approve(
        &self,
        _host: &RuntimeHost,
        iteration_id: &str,
    ) -> Result<crate::host::KernelPluginIterationResult, RuntimeError> {
        if iteration_id == "iter-fail" {
            return Err(RuntimeError::InvalidArgument {
                message: "runner refused".to_string(),
            });
        }
        Ok(canned_result(
            "iter-approved",
            crate::kernel::plugin_iteration::PluginIterationFinalVerdict::Promoted,
        ))
    }
}

/// Runs before `a01` in the serial order (libtest sorts by test name), so the
/// "no runner installed" branch is observable exactly once.
#[test]
#[serial]
fn a00_request_iteration_reports_a_missing_runner() {
    for () in crate::iteration_api::iteration_runner()
        .is_none()
        .then_some(())
        .into_iter()
    {
        let (_temp, host) = json_plugin_host();
        let caller = start_session(&host);
        let value = AgentToolHost::agent_request_iteration(&host, &caller, "/demo", "add a node")
            .expect("tool reports JSON");
        assert_eq!(value["ok"], false);
        assert_eq!(
            value["error"],
            "internal invariant broken: no plugin iteration runner installed (this build has no \
             iteration runner)"
        );

        // Approve uses the same seam, so its missing-runner arm is only
        // observable before `a01` installs one.
        host.refresh_session_caller(&caller, false, false, true)
            .expect("grant approval");
        let err = AgentToolHost::agent_approve_iteration(&host, &caller, "iter")
            .expect_err("no runner installed");
        assert_eq!(
            err.to_string(),
            "internal invariant broken: no plugin iteration runner installed (this build has no \
             iteration runner)"
        );
    }
}

/// Make sure a runner is live even when the tests that need one start
/// concurrently with the installer (the default parallel `cargo test` run).
/// `install_iteration_runner` is a `OnceLock`, so the first caller wins and the
/// rest observe it.
fn ensure_canned_runner() {
    if crate::iteration_api::iteration_runner().is_none() {
        let _ = crate::iteration_api::install_iteration_runner(Box::new(CannedIterationRunner));
    }
}

#[test]
#[serial]
fn a01_install_the_canned_iteration_runner() {
    ensure_canned_runner();
    assert!(
        crate::iteration_api::iteration_runner().is_some(),
        "some runner must be live for the approve/request tests"
    );
}

#[test]
#[serial]
fn a02_request_iteration_validates_the_plugin_path_and_returns_the_verdict() {
    ensure_canned_runner();
    let (_temp, host) = json_plugin_host();
    let caller = start_session(&host);

    // Unknown path: rejected before the runner is consulted.
    let value = AgentToolHost::agent_request_iteration(&host, &caller, "/nope", "x")
        .expect("tool reports JSON");
    assert_eq!(value["ok"], false);
    assert_eq!(
        value["error"],
        "unknown plugin path \"/nope\". Use \"/<plugin>\" for one plugin (e.g. \"/qq\") or \"/\" \
         for the whole workspace. Loaded plugins: cordis, demo, plain"
    );

    // A `plugins/`-prefixed path is normalised to the plugin path.
    let value = AgentToolHost::agent_request_iteration(&host, &caller, "plugins/demo", "x")
        .expect("tool reports JSON");
    assert_eq!(value["iteration_id"], "iter-fake");
    assert_eq!(value["final_verdict"], "Blocked");
    assert_eq!(value["ok"], false);

    // `/` is whole-workspace mode and carries no target plugin paths.
    let value =
        AgentToolHost::agent_request_iteration(&host, &caller, "/", "x").expect("root mode");
    assert_eq!(value["iteration_id"], "iter-fake");

    // A runner error is reported as JSON, not propagated.
    let value = AgentToolHost::agent_request_iteration(&host, &caller, "/demo", "fail")
        .expect("tool reports JSON");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"], "invalid argument: runner refused");
}

#[test]
#[serial]
fn approve_iteration_enforces_the_channel_grant() {
    let (_temp, host) = empty_host();
    // Unknown session: no caller identity at all.
    let err = AgentToolHost::agent_approve_iteration(&host, "unknown-session", "iter")
        .expect_err("no caller");
    assert_eq!(
        err.to_string(),
        "invalid argument: approve_iteration: unknown agent session (no caller identity)"
    );

    // A live session whose channel did not grant the capability.
    let session = start_session(&host);
    let err =
        AgentToolHost::agent_approve_iteration(&host, &session, "iter").expect_err("no capability");
    assert_eq!(
        err.to_string(),
        "invalid argument: approve_iteration: this speaker may not approve iterations. The \
         channel must report the capability (operator allowlist or the room's owner/admin); ask \
         an operator to approve it instead."
    );
}

#[test]
#[serial]
fn approve_iteration_promotes_when_the_channel_grants_the_capability() {
    ensure_canned_runner();
    let (_temp, host) = empty_host();
    let session = start_session(&host);
    host.refresh_session_caller(&session, false, false, true)
        .expect("grant approval");

    let value =
        AgentToolHost::agent_approve_iteration(&host, &session, "iter-ok").expect("approve");
    assert_eq!(value["ok"], true);
    assert_eq!(value["iteration_id"], "iter-approved");
    assert_eq!(value["verdict"], "Promoted");
    assert_eq!(value["changed_paths"], json!([]));
    assert_eq!(value["summary"], "canned iteration");

    // A runner refusal is propagated as an error.
    let err = AgentToolHost::agent_approve_iteration(&host, &session, "iter-fail")
        .expect_err("runner refuses");
    assert_eq!(err.to_string(), "invalid argument: runner refused");
}

// ─────────────────────── execute_agent_tool_call ──────────────────────────

struct TestBackend {
    host: RuntimeHost,
}

impl AgentBackend for TestBackend {
    type Host = RuntimeHost;

    fn system_prompt(&self) -> String {
        "test prompt".to_string()
    }

    fn tool_specs(&self) -> Vec<crate::agent_api::AgentToolSpec> {
        Vec::new()
    }

    fn execute_tool(
        &mut self,
        name: &str,
        arguments: Value,
        _attachments: &mut Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        if name == "fail" {
            return Err(RuntimeError::InvalidArgument {
                message: "boom".to_string(),
            });
        }
        Ok(arguments)
    }

    fn host(&self) -> &RuntimeHost {
        &self.host
    }

    fn tool_scope_label(&self) -> String {
        "runtime_shell".to_string()
    }
}

fn tool_call(name: &str, arguments: &str) -> cordis_plugin_sdk::llm::LlmToolCall {
    cordis_plugin_sdk::llm::LlmToolCall {
        id: "call-1".to_string(),
        call_type: "function".to_string(),
        function: cordis_plugin_sdk::llm::LlmToolFunction {
            name: name.to_string(),
            arguments: arguments.to_string(),
        },
    }
}

fn backend_and_tools() -> (TempDir, TestBackend, BTreeSet<String>) {
    let (temp, host) = empty_host();
    let backend = TestBackend { host };
    let mut tools = BTreeSet::new();
    tools.insert("read_file".to_string());
    tools.insert("replace_file_exact".to_string());
    tools.insert("fail".to_string());
    (temp, backend, tools)
}

#[test]
#[serial]
fn unknown_tool_call_is_reported_as_an_error() {
    let (_temp, mut backend, tools) = backend_and_tools();
    let mut strikes = 0;
    let mut attachments = Vec::new();
    let (event, text) = crate::agent_tools::execute_agent_tool_call(
        &mut backend,
        &tools,
        "runtime_shell",
        &tool_call("nope", "{}"),
        &mut strikes,
        &mut attachments,
    );
    assert!(!event.ok);
    assert_eq!(
        event.error.as_deref(),
        Some("tool nope is not available in the current runtime_shell scope (strike 0/1)")
    );
    assert!(
        !text.contains("STOP —"),
        "below the strike limit there is no stop block: {text}"
    );

    // At the limit the error text gains the stop block listing the tools.
    strikes = 1;
    let (event, text) = crate::agent_tools::execute_agent_tool_call(
        &mut backend,
        &tools,
        "runtime_shell",
        &tool_call("nope", "{}"),
        &mut strikes,
        &mut attachments,
    );
    assert!(!event.ok);
    assert!(
        text.contains("STOP — you have called 1 unsupported tools."),
        "text={text}"
    );
    assert!(
        text.contains("Your ONLY available tools are: fail, read_file, replace_file_exact."),
        "text={text}"
    );
}

#[test]
#[serial]
fn invalid_json_arguments_are_reported_with_a_recovery_hint() {
    let (_temp, mut backend, tools) = backend_and_tools();
    let mut strikes = 0;
    let mut attachments = Vec::new();

    let (event, text) = crate::agent_tools::execute_agent_tool_call(
        &mut backend,
        &tools,
        "runtime_shell",
        &tool_call("read_file", "{not json"),
        &mut strikes,
        &mut attachments,
    );
    assert!(!event.ok);
    assert!(
        event
            .error
            .as_deref()
            .expect("error")
            .starts_with("tool read_file received invalid JSON arguments: "),
        "event={event:?}"
    );
    assert!(text.starts_with('{') && text.contains("invalid JSON arguments"));

    // The batch-repair tools get the extra retry hint.
    let (event, _) = crate::agent_tools::execute_agent_tool_call(
        &mut backend,
        &tools,
        "runtime_shell",
        &tool_call("replace_file_exact", "{not json"),
        &mut strikes,
        &mut attachments,
    );
    assert!(
        event
            .error
            .as_deref()
            .expect("error")
            .contains("Retry with valid JSON."),
        "event={event:?}"
    );
}

#[test]
#[serial]
fn a_successful_tool_call_is_wrapped_and_an_empty_argument_string_means_an_empty_object() {
    let (_temp, mut backend, tools) = backend_and_tools();
    let mut strikes = 0;
    let mut attachments = Vec::new();

    let (event, text) = crate::agent_tools::execute_agent_tool_call(
        &mut backend,
        &tools,
        "runtime_shell",
        &tool_call("read_file", ""),
        &mut strikes,
        &mut attachments,
    );
    assert!(event.ok);
    assert_eq!(event.arguments, json!({}));
    assert_eq!(event.output, Some(json!({})));
    assert!(!text.is_empty());

    let (event, _) = crate::agent_tools::execute_agent_tool_call(
        &mut backend,
        &tools,
        "runtime_shell",
        &tool_call("fail", "{\"path\":\"a\"}"),
        &mut strikes,
        &mut attachments,
    );
    assert!(!event.ok);
    assert_eq!(event.error.as_deref(), Some("invalid argument: boom"));
    assert_eq!(event.arguments, json!({"path": "a"}));
}
