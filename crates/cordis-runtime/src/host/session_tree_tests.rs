//! The session organisation tree and the progressive-disclosure tools built on
//! it (`my_team` / `send_to_session`).
//!
//! What the code owns is one edge per session (`spawned_by`) plus the room it
//! was registered under. Everything else — who reports to whom, what each one
//! is responsible for — is something the model asks, not something the tree
//! declares. These tests pin the structural facts and the two rules that keep
//! the disclosure progressive: `my_team` returns neighbours only, and
//! `send_to_session` only addresses ids that appeared in the caller's latest
//! `my_team` reply.

use super::*;
use crate::agent_api::AgentToolHost;
use crate::core::models::{ArtifactIndexEntry, ArtifactKind, PluginArtifact};
use cordis_plugin_sdk::NodeDoc;
use serde_json::Value;
use serial_test::serial;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Hermetic fixture root. `with_provider` adds a JSON plugin declaring an
/// `llm_complete` node so `agent_send` gets past provider resolution without
/// touching the network (the kernel's test loop never calls it).
fn setup_fixture(with_provider: bool) -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");
    let mut entries: Vec<ArtifactIndexEntry> = Vec::new();
    let mut topo_order: Vec<String> = Vec::new();
    if with_provider {
        let abi = AbiFingerprint::current_build("crate_fake_llm_v1", "api_v2");
        let docs = PluginDocs {
            plugin_id: "fake_llm".to_string(),
            plugin_path: "fake_llm".to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 2,
            command_name: None,
            nodes: vec![NodeDoc {
                id: "llm_complete".to_string(),
                summary: "test provider".to_string(),
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                side_effects: Vec::new(),
                failure_modes: Vec::new(),
                node_type: NodeType::Task,
                agent_accessible: false,
            }],
            system_hint: None,
        };
        let artifact = PluginArtifact {
            plugin_path: "fake_llm".to_string(),
            abi_fingerprint: abi.clone(),
            docs: docs.clone(),
            exports: Vec::new(),
            execution: None,
        };
        let artifact_path = artifacts.join("fake_llm.json");
        fs::write(
            &artifact_path,
            serde_json::to_string_pretty(&artifact).expect("artifact json"),
        )
        .expect("write artifact");
        let sha256 = crate::plugin::artifact::sha256_file(&artifact_path).expect("sha256");
        entries.push(ArtifactIndexEntry {
            plugin_path: "fake_llm".to_string(),
            version: "0.1.0".to_string(),
            abi_fingerprint: abi,
            artifact_path: "fake_llm.json".to_string(),
            sha256,
            built_at: "0".to_string(),
            parent: None,
            required: true,
            grants_from_parent: Vec::new(),
            docs,
            exports: Vec::new(),
            execution: None,
            artifact_kind: ArtifactKind::Json,
            build_fingerprint: "bf".to_string(),
            input_probe: Default::default(),
            local_path_deps: Vec::new(),
        });
        topo_order.push("fake_llm".to_string());
    }
    let index = serde_json::json!({
        "schema_version": 2,
        "generated_at": "2026-07-25T00:00:00Z",
        "topo_order": topo_order,
        "entries": entries,
    });
    fs::write(
        artifacts.join("index.json"),
        serde_json::to_string_pretty(&index).expect("index json"),
    )
    .expect("write empty artifact index");
    (temp, fixtures)
}

fn empty_host() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = setup_fixture(false);
    let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");
    (temp, host)
}

/// A host that can run a turn (has a provider plugin) without any network.
fn provider_host() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = setup_fixture(true);
    let host = RuntimeHost::boot(&fixtures).expect("boot with provider");
    (temp, host)
}

fn start(host: &RuntimeHost) -> String {
    host.agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start a session")
        .session_id
}

/// A `PreparedPluginIteration` good enough to register an iteration session;
/// nothing here touches the filesystem until the pipeline runs a turn.
fn prepared_iteration() -> PreparedPluginIteration {
    PreparedPluginIteration {
        iteration_id: "iter-session-tree".to_string(),
        issue_id: "issue-session-tree".to_string(),
        root_plugin_path: "mini".to_string(),
        target_plugin_paths: vec!["mini".to_string()],
        source: None,
        summary: "session tree".to_string(),
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: crate::kernel::verifier::VerificationProfile::RustWorkspace,
        quality_score: None,
        edit_plan: None,
        instruction: None,
        allowed_plugin_roots: BTreeMap::from([("mini".to_string(), "plugins/mini".to_string())]),
    }
}

fn start_iteration(host: &RuntimeHost) -> String {
    host.start_iteration_session(
        prepared_iteration(),
        PluginIterationContextPaths {
            focus_paths: Vec::new(),
            all_paths: Vec::new(),
        },
    )
    .expect("start an iteration session")
}

/// The ids of a JSON array, sorted: the host emits them in `BTreeMap` order,
/// and asserting on a sorted list keeps the test independent of id minting.
fn strings(value: &Value) -> Vec<String> {
    let mut ids: Vec<String> = value
        .as_array()
        .expect("expected a JSON array")
        .iter()
        .map(|item| item.as_str().expect("expected ids").to_string())
        .collect();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// Recursively collect every object key in a JSON value.
fn object_keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                out.push(key.clone());
                object_keys(nested, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                object_keys(item, out);
            }
        }
        _ => {}
    }
}

// ─────────────── tree construction ───────────────────────────────────

#[test]
#[serial]
fn the_main_session_is_a_root_and_rooms_hang_off_it() {
    let (_temp, host) = empty_host();
    let main = start(&host);
    host.set_main_session(&main).expect("main session");
    // Re-registering the same main is a no-op, not an error.
    host.set_main_session(&main).expect("idempotent main");

    let room = start(&host);
    host.agent_set_session_address(&room, "qq:group:867130587")
        .expect("address");
    host.attach_session_to_main(&room).expect("attach");
    // A channel re-registers the address on every message; the existing
    // parent must be left untouched.
    host.agent_set_session_address(&room, "qq:group:867130587")
        .expect("address again");

    assert_eq!(host.session_parent(&room).as_deref(), Some(main.as_str()));
    assert_eq!(host.session_parent(&main), None);
    // Attaching the main session to itself is refused by the helper, which
    // returns early instead of recording a self-edge.
    host.attach_session_to_main(&main)
        .expect("main is its own root");

    let team = host.agent_my_team(&main).expect("main team");
    assert_eq!(team["me"], main);
    assert_eq!(team["spawned_by"], Value::Null);
    assert_eq!(strings(&team["created"]), vec![room.clone()]);
    assert_eq!(strings(&team["reachable"]), sorted(vec![room]));

    let team = host.agent_my_team(&main).expect("main team");
    assert!(strings(&team["same_room"]).is_empty());
}

#[test]
#[serial]
fn an_iteration_session_hangs_off_the_main_session() {
    let (_temp, host) = empty_host();
    // No main registered yet: the edge falls back to being a root instead of
    // failing the iteration, and attaching is a no-op rather than an error.
    let orphan_iteration = start_iteration(&host);
    host.attach_session_to_main(&orphan_iteration)
        .expect("no main to attach to");
    assert_eq!(host.session_parent(&orphan_iteration), None);

    let main = start(&host);
    host.set_main_session(&main).expect("main");
    let iteration = start_iteration(&host);
    assert_eq!(
        host.session_parent(&iteration).as_deref(),
        Some(main.as_str())
    );

    let team = host.agent_my_team(&main).expect("main team");
    assert!(strings(&team["created"]).contains(&iteration));
}

#[test]
#[serial]
fn main_session_registration_refuses_an_unknown_session() {
    let (_temp, host) = empty_host();
    let err = host
        .set_main_session("no-such-session")
        .expect_err("unknown main");
    assert!(err.to_string().contains("not live"), "got {err}");
    assert_eq!(host.main_session_id(), None);
}

#[test]
#[serial]
fn parent_edges_refuse_self_parents_and_dead_endpoints() {
    let (_temp, host) = empty_host();
    let victim = start(&host);
    let err = host
        .set_session_parent(&victim, &victim)
        .expect_err("self parent");
    assert!(err.to_string().contains("own parent"), "got {err}");

    let child = start(&host);
    let err = host
        .set_session_parent(&child, "no-such-parent")
        .expect_err("dead parent");
    assert!(err.to_string().contains("no-such-parent"), "got {err}");

    let err = host
        .set_session_parent("no-such-child", &victim)
        .expect_err("dead child");
    assert!(err.to_string().contains("no-such-child"), "got {err}");
    assert_eq!(host.session_parent(&child), None);
}

// ─────────────── my_team: neighbours, dedup, no labels ───────────────

#[test]
#[serial]
fn my_team_returns_neighbours_not_the_whole_session_table() {
    let (_temp, host) = empty_host();
    let main = start(&host);
    host.set_main_session(&main).expect("main");

    let group_one = start(&host);
    host.agent_set_session_address(&group_one, "qq:group:1")
        .expect("address");
    host.attach_session_to_main(&group_one).expect("attach");

    let group_two = start(&host);
    host.agent_set_session_address(&group_two, "qq:group:2")
        .expect("address");
    host.attach_session_to_main(&group_two).expect("attach");

    let private_chat = start(&host);
    host.agent_set_session_address(&private_chat, "qq:private:9")
        .expect("address");
    host.attach_session_to_main(&private_chat).expect("attach");

    // An unrelated live session: another channel, no edge to this subtree.
    let unrelated = start(&host);
    host.agent_set_session_address(&unrelated, "feishu:chat:oc_x")
        .expect("address");

    let team = host.agent_my_team(&group_one).expect("group team");
    assert_eq!(team["spawned_by"], main);
    assert!(strings(&team["created"]).is_empty());
    assert_eq!(strings(&team["same_room"]), vec![group_two.clone()]);
    let reachable = strings(&team["reachable"]);
    assert_eq!(reachable, sorted(vec![main.clone(), group_two.clone()]));
    // The whole point: everyone else is absent, not merely deprioritised.
    for unrelated_id in [&private_chat, &unrelated, &group_one] {
        assert!(
            !reachable.contains(unrelated_id),
            "{unrelated_id} leaked into {reachable:?}"
        );
    }
}

#[test]
#[serial]
fn reachable_is_deduplicated_and_never_contains_the_caller() {
    let (_temp, host) = empty_host();
    let main = start(&host);
    host.set_main_session(&main).expect("main");
    // Give the main session a room of its own, so its child is both created
    // and same-room: the union must still list it once.
    host.agent_set_session_address(&main, "qq:group:0")
        .expect("address");

    let child = start(&host);
    host.agent_set_session_address(&child, "qq:group:1")
        .expect("address");
    host.attach_session_to_main(&child).expect("attach");

    let team = host.agent_my_team(&main).expect("main team");
    assert_eq!(strings(&team["created"]), vec![child.clone()]);
    assert_eq!(strings(&team["same_room"]), vec![child.clone()]);
    assert_eq!(strings(&team["reachable"]), sorted(vec![child.clone()]));
    assert!(!strings(&team["reachable"]).contains(&main));

    let team = host.agent_my_team(&child).expect("child team");
    assert_eq!(team["spawned_by"], main);
    // `main` arrives through both `spawned_by` and `same_room`.
    assert_eq!(strings(&team["same_room"]), sorted(vec![main.clone()]));
    assert_eq!(strings(&team["reachable"]), sorted(vec![main.clone()]));
    assert!(!strings(&team["reachable"]).contains(&child));
}

#[test]
#[serial]
fn a_dead_parent_is_dropped_instead_of_leaked() {
    let (_temp, host) = empty_host();
    let main = start(&host);
    host.set_main_session(&main).expect("main");
    let child = start(&host);
    host.attach_session_to_main(&child).expect("attach");
    host.drop_session(&main);

    let team = host.agent_my_team(&child).expect("child team");
    assert_eq!(team["spawned_by"], Value::Null);
    assert!(strings(&team["reachable"]).is_empty());
}

#[test]
#[serial]
fn my_team_uses_only_neutral_structural_keys() {
    let (_temp, host) = empty_host();
    let main = start(&host);
    host.set_main_session(&main).expect("main");
    let child = start(&host);
    host.agent_set_session_address(&child, "qq:group:1")
        .expect("address");
    host.attach_session_to_main(&child).expect("attach");

    let team = host.agent_my_team(&child).expect("team");
    let mut keys: Vec<String> = Vec::new();
    object_keys(&team, &mut keys);
    for banned in [
        "role",
        "peer",
        "supervisor",
        "manager",
        "sibling",
        "上级",
        "同级",
    ] {
        assert!(
            !keys.iter().any(|key| key == banned),
            "my_team must not label people: found key {banned} in {keys:?}"
        );
    }
    // The structural keys that must be there, so the test cannot pass by
    // returning an empty object.
    for required in ["me", "spawned_by", "created", "same_room", "reachable"] {
        assert!(keys.iter().any(|key| key == required), "missing {required}");
    }
}

#[test]
#[serial]
fn a_room_kind_without_a_colon_is_the_address_itself() {
    let (_temp, host) = empty_host();
    let first = start(&host);
    host.agent_set_session_address(&first, "legacy")
        .expect("address");
    let second = start(&host);
    host.agent_set_session_address(&second, "legacy")
        .expect("address");

    let team = host.agent_my_team(&first).expect("team");
    assert_eq!(strings(&team["same_room"]), vec![second]);
}

// ─────────────── send_to_session gating ──────────────────────────────

#[test]
#[serial]
fn send_to_session_is_refused_until_my_team_reveals_the_target() {
    let (_temp, host) = provider_host();
    let main = start(&host);
    host.set_main_session(&main).expect("main");
    host.agent_set_session_address(&main, "qq:group:0")
        .expect("address");

    let room = start(&host);
    host.agent_set_session_address(&room, "qq:group:1")
        .expect("address");
    host.attach_session_to_main(&room).expect("attach");

    // A live session with no edge to the caller: it must stay unknown.
    let stranger = start(&host);

    // Never asked my_team: even a real neighbour is refused.
    let err = host
        .agent_send_to_session(&main, &room, "hi")
        .expect_err("not introduced yet");
    assert!(err.to_string().contains("my_team"), "got {err}");

    // Unknown id and known-but-unreachable id both point at my_team.
    let err = host
        .agent_send_to_session(&main, "no-such-id", "hi")
        .expect_err("unknown id");
    assert!(err.to_string().contains("my_team"), "got {err}");

    let team = host.agent_my_team(&main).expect("main team");
    assert!(!strings(&team["reachable"]).contains(&stranger));
    let err = host
        .agent_send_to_session(&main, &stranger, "hi")
        .expect_err("unreachable id");
    assert!(err.to_string().contains("my_team"), "got {err}");
    assert_eq!(
        err.to_string(),
        format!(
            "invalid argument: unknown session {stranger}: it is not in your reachable set. \
             Call my_team first to see who you know."
        )
    );

    // Now that my_team revealed it, the send goes through — by id and by
    // address. Peers are allowed to talk directly; there is no hierarchy gate.
    host.agent_send_to_session(&main, &room, "hi")
        .expect("reachable by id");
    let by_address = host
        .agent_send_to_session(&main, "qq:group:1", "hi again")
        .expect("reachable by address");
    assert_eq!(by_address["ok"], true);
    assert_eq!(by_address["session_id"], room);
}

/// The tools reach the tree through the `AgentToolHost` trait, not through the
/// inherent methods the tests above use. Without this the forwarding layer
/// could rot silently while every inherent-method test still passes.
#[test]
#[serial]
fn the_tool_host_trait_forwards_to_the_session_tree() {
    let (_temp, host) = provider_host();
    let main = start(&host);
    host.set_main_session(&main).expect("main");
    let room = start(&host);
    host.agent_set_session_address(&room, "qq:group:7")
        .expect("address");
    host.attach_session_to_main(&room).expect("attach");

    let team = AgentToolHost::agent_my_team(&host, &main).expect("trait team");
    assert_eq!(team["me"], main);
    assert_eq!(strings(&team["reachable"]), vec![room.clone()]);

    let sent =
        AgentToolHost::agent_send_to_session(&host, &main, &room, "ping").expect("trait send");
    assert_eq!(sent["ok"], true);
    assert_eq!(sent["session_id"], room);
}

// ─────────────── liveness: recovered history stays invisible ─────────

/// Write one crash-recovery snapshot into the fixture tree's data dir.
fn seed_orphan_snapshot(fixtures: &Path, session_id: &str) {
    let sessions = fixtures
        .parent()
        .expect("fixtures has a parent")
        .join("data/sessions");
    fs::create_dir_all(&sessions).expect("sessions dir");
    let snapshot = AgentSessionSnapshot {
        kind: "runtime_shell".to_string(),
        config: LlmApiConfig::default(),
        history: Vec::new(),
        transcript: Vec::new(),
        completed_turns: 0,
        estimated_tokens: 0,
        require_end_tool: false,
        soul_key: String::new(),
    };
    fs::write(
        sessions.join(format!("{session_id}.json")),
        serde_json::to_vec(&snapshot).expect("snapshot json"),
    )
    .expect("write snapshot");
}

#[test]
#[serial]
fn a_recovered_history_session_is_invisible_until_it_is_actually_used() {
    let (_temp, fixtures) = setup_fixture(true);
    seed_orphan_snapshot(&fixtures, "agent-session-orphan-1");
    let host = RuntimeHost::boot(&fixtures).expect("boot with history");

    // It was read back into the session map…
    assert!(host.agent_status("agent-session-orphan-1").is_ok());
    // …but it is not a teammate anyone can reach.
    let err = host
        .agent_my_team("agent-session-orphan-1")
        .expect_err("history is not live");
    assert!(
        err.to_string().contains("agent-session-orphan-1"),
        "got {err}"
    );

    let main = start(&host);
    host.set_main_session(&main).expect("main");
    host.agent_set_session_address(&main, "qq:group:0")
        .expect("address");
    let team = host.agent_my_team(&main).expect("main team");
    assert!(strings(&team["reachable"]).is_empty());
    assert!(strings(&team["same_room"]).is_empty());
    assert!(strings(&team["created"]).is_empty());

    // The moment it runs a turn it is a live teammate again.
    host.agent_send("agent-session-orphan-1", "hello")
        .expect("turn");
    let team = host
        .agent_my_team("agent-session-orphan-1")
        .expect("now live");
    assert_eq!(team["me"], "agent-session-orphan-1");
}

#[test]
#[serial]
fn a_recovered_room_session_joins_the_tree_when_its_address_is_registered() {
    let (_temp, fixtures) = setup_fixture(false);
    seed_orphan_snapshot(&fixtures, "agent-session-room-1");
    let host = RuntimeHost::boot(&fixtures).expect("boot with history");
    let main = start(&host);
    host.set_main_session(&main).expect("main");

    // Registering the address is how a channel says "this room is back".
    host.agent_set_session_address("agent-session-room-1", "qq:group:42")
        .expect("address");
    assert_eq!(
        host.session_parent("agent-session-room-1").as_deref(),
        Some(main.as_str())
    );
    let team = host.agent_my_team("agent-session-room-1").expect("live");
    assert_eq!(team["spawned_by"], main);
}
