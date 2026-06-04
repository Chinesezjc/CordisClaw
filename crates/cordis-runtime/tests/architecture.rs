use cordis_runtime::context::ContextRegistry;
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::{
    ArtifactKind, NodeOutcome, PluginLoadResult, PluginUnavailableReason,
};
use cordis_runtime::execution::scheduler::{run_deterministic, ScheduledNode, SchedulerConfig};
use cordis_runtime::plugin::invoke::PluginInvoker;
use cordis_runtime::plugin::loader::{default_loader_config, Loader};
use cordis_runtime::plugin::registry::{NodeRegistry, PluginRegistry};
use cordis_runtime::plugin::tooling::{prepare_artifacts, PrepareMode};
use cordis_runtime::service::graph_registry::GraphRegistry;
use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, PluginDocs};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

mod support;

use support::fixtures_root;

// ---------------------------------------------------------------------------
// test helpers for building registries directly
// ---------------------------------------------------------------------------

fn dummy_abi() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "test".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "deadbeef".to_string(),
        api_hash: "cafebabe".to_string(),
    }
}

fn make_plugin_docs(plugin_path: &str, nodes: Vec<NodeDoc>) -> PluginDocs {
    PluginDocs {
        plugin_id: plugin_path.replace('/', "_"),
        plugin_path: plugin_path.to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 1,
        command_name: None,
        nodes,
    }
}

fn make_node_doc(id: &str, consumes: &[&str], produces: &[&str]) -> NodeDoc {
    let mut input_props = serde_json::Map::new();
    for field in consumes {
        input_props.insert(field.to_string(), json!({"type": "string"}));
    }
    let mut output_props = serde_json::Map::new();
    for field in produces {
        output_props.insert(field.to_string(), json!({"type": "string"}));
    }
    NodeDoc {
        id: id.to_string(),
        summary: format!("test node {id}"),
        input_schema: json!({"type": "object", "properties": input_props}),
        output_schema: json!({"type": "object", "properties": output_props}),
        side_effects: vec![],
        failure_modes: vec![],
        node_type: Default::default(),
    }
}

fn insert_test_plugin(
    plugin_registry: &PluginRegistry,
    plugin_path: &str,
    docs: &PluginDocs,
) {
    plugin_registry.insert_loaded(
        plugin_path.to_string(),
        None,
        true,
        BTreeSet::new(),
        docs.clone(),
        PathBuf::from("/tmp/test.so"),
        ArtifactKind::Dylib,
        dummy_abi(),
        None,
    );
}

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
    let src = fixtures_root();
    copy_dir_all(&src, temp.path());
    temp
}

fn patch_index<F>(temp: &TempDir, patch: F)
where
    F: FnOnce(&mut Value),
{
    let index_path = temp.path().join("artifacts/index.json");
    let content = fs::read_to_string(&index_path).expect("read index");
    let mut value: Value = serde_json::from_str(&content).expect("parse index");
    patch(&mut value);
    let patched = serde_json::to_string_pretty(&value).expect("serialize index");
    fs::write(&index_path, patched).expect("write patched index");
}

#[test]
fn load_success_and_grants_enforced() {
    let temp = setup_fixture_copy();
    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);

    let output = loader.load().expect("load should pass");

    assert!(matches!(
        output.plugin_registry.get("root").unwrap().load_result,
        PluginLoadResult::Loaded
    ));
    assert!(matches!(
        output
            .plugin_registry
            .get("root/child")
            .unwrap()
            .load_result,
        PluginLoadResult::Loaded
    ));
    assert!(matches!(
        output.plugin_registry.get("shell").unwrap().load_result,
        PluginLoadResult::Loaded
    ));
    assert!(output.node_registry.contains("root::root_entry"));
    assert!(output.node_registry.contains("root/child::child_entry"));
    assert!(output.node_registry.contains("shell::shell_entry"));

    let plugin_docs = output
        .doc_registry
        .get_plugin_docs("root")
        .expect("root docs should exist");
    assert_eq!(plugin_docs.plugin_path, "root");

    let node_docs = output
        .doc_registry
        .get_node_docs("root/child", "child_entry")
        .expect("child node docs should exist");
    assert_eq!(node_docs.id, "child_entry");

    let route_value = output
        .doc_registry
        .handle_get("/plugins/root/child/nodes/child_entry/docs")
        .expect("route query should succeed");
    assert_eq!(
        route_value.get("id").and_then(|x| x.as_str()),
        Some("child_entry")
    );

    let allowed = output
        .context
        .inject::<String>("root/child", "service.db")
        .expect("service.db should be granted");
    assert!(allowed.contains("service:root:service.db"));

    let denied = output
        .context
        .inject::<String>("root/child", "service.cache");
    assert!(matches!(denied, Err(RuntimeError::PermissionDenied { .. })));
}

#[test]
fn registered_graph_json_and_html_are_available() {
    let temp = setup_fixture_copy();
    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);
    let output = loader.load().expect("load should pass");

    let graph_json = output
        .graph_registry
        .handle_get_json("/graphs/registered-nodes")
        .expect("graph json should exist");
    let plugins = graph_json
        .get("plugins")
        .and_then(|value| value.as_array())
        .expect("plugins array");
    let nodes = graph_json
        .get("nodes")
        .and_then(|value| value.as_array())
        .expect("nodes array");

    assert!(plugins
        .iter()
        .any(|plugin| plugin.get("plugin_path").and_then(|v| v.as_str()) == Some("expr")));
    assert!(plugins
        .iter()
        .any(|plugin| plugin.get("plugin_path").and_then(|v| v.as_str()) == Some("shell")));
    assert!(nodes
        .iter()
        .any(|node| { node.get("node_fqn").and_then(|v| v.as_str()) == Some("expr::expr_entry") }));

    let html = output
        .graph_registry
        .handle_get_html("/graphs/registered-nodes.html")
        .expect("graph html should exist");
    assert!(html.contains("<!doctype html>"));
    assert!(html.contains("Registered Nodes Graph"));
    assert!(html.contains("expr::expr_entry"));
    assert!(html.contains("root/child::child_entry"));
    assert!(html.contains("shell::shell_entry"));
}

#[test]
fn registered_net_json_and_html_are_available() {
    let temp = setup_fixture_copy();
    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);
    let output = loader.load().expect("load should pass");

    let net_json = output
        .graph_registry
        .handle_get_json("/graphs/registered-net")
        .expect("net json should exist");
    let nodes = net_json
        .get("nodes")
        .and_then(|value| value.as_array())
        .expect("nodes array");
    let edges = net_json
        .get("edges")
        .and_then(|value| value.as_array())
        .expect("edges array");

    assert!(
        nodes.iter().any(|node| {
            node.get("node_fqn").and_then(|v| v.as_str()) == Some("expr/lexer::expr_lexer")
        }),
        "lexer node should appear in net"
    );
    assert!(
        nodes.iter().any(|node| {
            node.get("node_fqn").and_then(|v| v.as_str()) == Some("expr/parser::expr_parser")
        }),
        "parser node should appear in net"
    );
    assert!(
        nodes.iter().any(|node| {
            node.get("node_fqn").and_then(|v| v.as_str()) == Some("expr/evaluator::expr_evaluator")
        }),
        "evaluator node should appear in net"
    );
    assert!(
        edges.iter().any(|edge| {
            edge.get("from").and_then(|v| v.as_str()) == Some("expr/lexer::expr_lexer")
                && edge.get("to").and_then(|v| v.as_str()) == Some("expr/parser::expr_parser")
        }),
        "lexer -> parser edge should be inferred"
    );
    assert!(
        edges.iter().any(|edge| {
            edge.get("from").and_then(|v| v.as_str()) == Some("expr/parser::expr_parser")
                && edge.get("to").and_then(|v| v.as_str()) == Some("expr/evaluator::expr_evaluator")
        }),
        "parser -> evaluator edge should be inferred"
    );

    let html = output
        .graph_registry
        .handle_get_html("/graphs/registered-net.html")
        .expect("net html should exist");
    assert!(html.contains("<!doctype html>"));
    assert!(html.contains("Registered Net"));
    assert!(html.contains("expr/lexer::expr_lexer"));
    assert!(html.contains("expr/evaluator::expr_evaluator"));
}

#[test]
fn expr_dylib_subplugins_are_invokable() {
    let temp = setup_fixture_copy();
    let invoker = PluginInvoker::load(temp.path()).expect("fixtures should load");

    let lexer = invoker
        .invoke(
            "expr/lexer",
            "expr_lexer",
            json!({ "expression": "1 + 2 * 3" }).to_string(),
        )
        .expect("lexer should be invokable");
    let lexer_value: Value = serde_json::from_str(&lexer.payload).expect("lexer json");
    let tokens = lexer_value.get("tokens").cloned().expect("tokens field");

    let parser = invoker
        .invoke(
            "expr/parser",
            "expr_parser",
            json!({ "tokens": tokens }).to_string(),
        )
        .expect("parser should be invokable");
    let parser_value: Value = serde_json::from_str(&parser.payload).expect("parser json");
    let ast = parser_value.get("ast").cloned().expect("ast field");

    let evaluator = invoker
        .invoke(
            "expr/evaluator",
            "expr_evaluator",
            json!({ "ast": ast }).to_string(),
        )
        .expect("evaluator should be invokable");
    let evaluator_value: Value = serde_json::from_str(&evaluator.payload).expect("evaluator json");
    assert_eq!(
        evaluator_value.get("value").and_then(|v| v.as_f64()),
        Some(7.0)
    );

    let add = invoker
        .invoke(
            "expr/evaluator/add",
            "expr_add",
            json!({ "lhs": 1.0, "rhs": 2.0 }).to_string(),
        )
        .expect("add should be invokable");
    let add_value: Value = serde_json::from_str(&add.payload).expect("add json");
    assert_eq!(add_value.get("value").and_then(|v| v.as_f64()), Some(3.0));

    let div = invoker
        .invoke(
            "expr/evaluator/div",
            "expr_div",
            json!({ "lhs": 1.0, "rhs": 0.0 }).to_string(),
        )
        .expect("div should be invokable");
    let div_value: Value = serde_json::from_str(&div.payload).expect("div json");
    assert_eq!(
        div_value.get("error").and_then(|v| v.as_str()),
        Some("division by zero")
    );
}

#[test]
fn undeclared_grandchild_is_not_discovered() {
    let temp = setup_fixture_copy();

    let ghost_dir = temp.path().join("plugins/root/ghost");
    fs::create_dir_all(ghost_dir.join("src")).expect("mkdir ghost src");
    fs::create_dir_all(ghost_dir.join("tests")).expect("mkdir ghost tests");
    fs::create_dir_all(ghost_dir.join("docs/agent")).expect("mkdir ghost docs agent");
    fs::create_dir_all(ghost_dir.join("docs/human")).expect("mkdir ghost docs human");
    fs::write(
        ghost_dir.join("Cargo.toml"),
        r#"
[package]
name = "root_ghost"
version = "0.1.0"
edition = "2021"

[package.metadata.cordis]
plugin_path = "root/ghost"
abi_kind = "rust"
children = []

[package.metadata.cordis.abi_fingerprint]
rustc_version = "1.85.1"
target_triple = "x86_64-unknown-linux-gnu"
crate_hash = "crate_ghost_v1"
api_hash = "api_v2"
"#,
    )
    .expect("write ghost cargo");
    fs::write(
        ghost_dir.join("docs/agent/interfaces.json"),
        r#"{"plugin_id":"ghost","plugin_path":"root/ghost","plugin_version":"0.1.0","abi_version":2,"nodes":[]}"#,
    )
    .expect("write ghost interfaces");
    fs::write(ghost_dir.join("docs/human/overview.md"), "ghost").expect("write ghost docs");

    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);
    let output = loader.load().expect("load should pass");

    assert!(output.plugin_registry.get("root/ghost").is_none());
}

#[test]
fn child_path_escape_fails_fast() {
    let temp = setup_fixture_copy();
    let root_manifest = temp.path().join("plugins/root/Cargo.toml");
    let content = fs::read_to_string(&root_manifest).expect("read root manifest");
    let patched = content.replace("./child", "../child");
    fs::write(&root_manifest, patched).expect("write root manifest");

    let err = prepare_artifacts(temp.path(), PrepareMode::Incremental)
        .expect_err("must fail due to path escape");
    assert!(matches!(err, RuntimeError::InvalidChildSource { .. }));
}

#[test]
fn plugin_path_mismatch_fails_fast() {
    let temp = setup_fixture_copy();
    let child_manifest = temp.path().join("plugins/root/child/Cargo.toml");
    let content = fs::read_to_string(&child_manifest).expect("read child manifest");
    let patched = content.replace("plugin_path = \"root/child\"", "plugin_path = \"root/bad\"");
    fs::write(&child_manifest, patched).expect("write child manifest");

    let err = prepare_artifacts(temp.path(), PrepareMode::Incremental)
        .expect_err("must fail due to plugin_path mismatch");
    assert!(matches!(err, RuntimeError::PluginPathMismatch { .. }));
}

#[test]
fn optional_child_unavailable_does_not_block_parent() {
    let temp = setup_fixture_copy();

    let index_path = temp.path().join("artifacts/index.json");
    let index_content = fs::read_to_string(&index_path).expect("read index");
    let broken = index_content.replace("crate_child_v1", "crate_child_wrong");
    fs::write(&index_path, broken).expect("write broken index");
    patch_index(&temp, |index| {
        let entries = index
            .get_mut("entries")
            .and_then(|x| x.as_array_mut())
            .expect("entries array");
        let child = entries
            .iter_mut()
            .find(|x| x.get("plugin_path").and_then(|v| v.as_str()) == Some("root/child"))
            .expect("child entry");
        child["required"] = Value::Bool(false);
    });

    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);
    let output = loader
        .load()
        .expect("optional child failure should not abort");

    assert!(matches!(
        output.plugin_registry.get("root").unwrap().load_result,
        PluginLoadResult::Loaded
    ));
    assert!(matches!(
        output
            .plugin_registry
            .get("root/child")
            .unwrap()
            .load_result,
        PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch)
    ));
    assert!(output.metrics.dylib_no_fallback_total >= 1);
}

#[test]
fn required_child_unavailable_blocks_parent_chain() {
    let temp = setup_fixture_copy();

    patch_index(&temp, |index| {
        let entries = index
            .get_mut("entries")
            .and_then(|x| x.as_array_mut())
            .expect("entries array");
        let child = entries
            .iter_mut()
            .find(|x| x.get("plugin_path").and_then(|v| v.as_str()) == Some("root/child"))
            .expect("child entry");
        child["abi_fingerprint"]["crate_hash"] = Value::String("crate_child_wrong".to_string());
    });

    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);
    let output = loader
        .load()
        .expect("loader should continue with unavailable state");

    assert!(matches!(
        output
            .plugin_registry
            .get("root/child")
            .unwrap()
            .load_result,
        PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch)
    ));
    assert!(matches!(
        output.plugin_registry.get("root").unwrap().load_result,
        PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed)
    ));
}

#[test]
fn hash_mismatch_marks_child_unavailable_and_no_fallback() {
    let temp = setup_fixture_copy();

    patch_index(&temp, |index| {
        let entries = index
            .get_mut("entries")
            .and_then(|x| x.as_array_mut())
            .expect("entries array");
        let child = entries
            .iter_mut()
            .find(|x| x.get("plugin_path").and_then(|v| v.as_str()) == Some("root/child"))
            .expect("child entry");
        child["sha256"] = Value::String("deadbeef".to_string());
    });

    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);
    let output = loader
        .load()
        .expect("loader should continue with unavailable state");

    assert!(matches!(
        output
            .plugin_registry
            .get("root/child")
            .unwrap()
            .load_result,
        PluginLoadResult::Unavailable(PluginUnavailableReason::HashMismatch)
    ));
    assert!(matches!(
        output.plugin_registry.get("root").unwrap().load_result,
        PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed)
    ));
    assert!(output.metrics.dylib_no_fallback_total >= 1);
}

#[test]
fn inject_on_unavailable_plugin_returns_unavailable_error() {
    let temp = setup_fixture_copy();

    patch_index(&temp, |index| {
        let entries = index
            .get_mut("entries")
            .and_then(|x| x.as_array_mut())
            .expect("entries array");
        let child = entries
            .iter_mut()
            .find(|x| x.get("plugin_path").and_then(|v| v.as_str()) == Some("root/child"))
            .expect("child entry");
        child["abi_fingerprint"]["crate_hash"] = Value::String("crate_child_wrong".to_string());
    });

    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);
    let output = loader
        .load()
        .expect("loader should continue with unavailable state");

    assert!(matches!(
        output.context.inject::<String>("root/child", "service.db"),
        Err(RuntimeError::ContextPluginUnavailable { .. })
    ));
}

#[test]
fn scheduler_is_deterministic_across_runs() {
    let nodes = vec![
        ScheduledNode {
            id: "a".to_string(),
            topo_level: 0,
            priority: 1,
            deps: vec![],
            max_retries: 1,
        },
        ScheduledNode {
            id: "b".to_string(),
            topo_level: 1,
            priority: 10,
            deps: vec!["a".to_string()],
            max_retries: 0,
        },
        ScheduledNode {
            id: "c".to_string(),
            topo_level: 1,
            priority: 5,
            deps: vec!["a".to_string()],
            max_retries: 0,
        },
    ];

    let run_once = || {
        run_deterministic(
            SchedulerConfig { max_parallelism: 1, max_concurrency: 1 },
            nodes.clone(),
            |node, attempt| {
                if node.id == "a" && attempt == 0 {
                    NodeOutcome::Failure
                } else {
                    NodeOutcome::Success
                }
            },
        )
    };

    let first = run_once();
    let second = run_once();

    assert_eq!(first.order, second.order);
    assert_eq!(first.outcomes, second.outcomes);
    assert_eq!(first.order, vec!["a", "a", "b", "c"]);
}

// ---------------------------------------------------------------------------
// registered-net edge-case tests (direct GraphRegistry construction)
// ---------------------------------------------------------------------------

#[test]
fn empty_net_from_empty_registries() {
    let plugin_registry = PluginRegistry::default();
    let node_registry = NodeRegistry::default();
    let graph_registry = GraphRegistry::from_registries(&plugin_registry, &node_registry);
    let net = graph_registry.net();
    assert!(net.nodes.is_empty());
    assert!(net.edges.is_empty());
    assert!(net.diagnostics.is_empty());
}

#[test]
fn multi_producer_input_generates_warning() {
    let plugin_registry = PluginRegistry::default();
    let mut node_registry = NodeRegistry::default();

    // Two producers of "result"
    let producer_a = make_node_doc("producer_a", &[], &["result"]);
    let producer_b = make_node_doc("producer_b", &[], &["result"]);
    let consumer = make_node_doc("consumer", &["result"], &[]);

    let docs_a = make_plugin_docs("pkg/a", vec![producer_a]);
    let docs_b = make_plugin_docs("pkg/b", vec![producer_b]);
    let docs_c = make_plugin_docs("pkg/c", vec![consumer]);

    insert_test_plugin(&plugin_registry, "pkg/a", &docs_a);
    insert_test_plugin(&plugin_registry, "pkg/b", &docs_b);
    insert_test_plugin(&plugin_registry, "pkg/c", &docs_c);

    node_registry
        .register_from_docs("pkg/a", &docs_a)
        .unwrap();
    node_registry
        .register_from_docs("pkg/b", &docs_b)
        .unwrap();
    node_registry
        .register_from_docs("pkg/c", &docs_c)
        .unwrap();

    let graph_registry = GraphRegistry::from_registries(&plugin_registry, &node_registry);
    let net = graph_registry.net();

    // Consumer should appear
    assert!(net.nodes.iter().any(|n| n.node_fqn == "pkg/c::consumer"));

    // One Data edge should exist: the alphabetically first producer wins.
    let consumer_edges: Vec<_> = net
        .edges
        .iter()
        .filter(|e| e.to == "pkg/c::consumer")
        .collect();
    assert_eq!(consumer_edges.len(), 1);

    // A diagnostic about multiple producers should be emitted.
    let has_multi = net
        .diagnostics
        .iter()
        .any(|d| d.contains("multiple producers") && d.contains("result"));
    assert!(has_multi, "expected multi-producer diagnostic, got: {net:?}");
}

#[test]
fn cycle_detection_produces_diagnostic() {
    let plugin_registry = PluginRegistry::default();
    let mut node_registry = NodeRegistry::default();

    // A produces x, consumes z
    let node_a = make_node_doc("a", &["z"], &["x"]);
    // B produces y, consumes x
    let node_b = make_node_doc("b", &["x"], &["y"]);
    // C produces z, consumes y
    let node_c = make_node_doc("c", &["y"], &["z"]);

    let docs = make_plugin_docs("cycle", vec![node_a, node_b, node_c]);
    insert_test_plugin(&plugin_registry, "cycle", &docs);
    node_registry
        .register_from_docs("cycle", &docs)
        .unwrap();

    let graph_registry = GraphRegistry::from_registries(&plugin_registry, &node_registry);
    let net = graph_registry.net();

    // All three nodes should appear.
    assert_eq!(net.nodes.len(), 3);

    // A cycle diagnostic should be emitted.
    let has_cycle = net
        .diagnostics
        .iter()
        .any(|d| d.contains("cycle-like dependencies"));
    assert!(has_cycle, "expected cycle diagnostic, got: {net:?}");
}

#[test]
fn orphan_node_appears_without_edges() {
    let plugin_registry = PluginRegistry::default();
    let mut node_registry = NodeRegistry::default();

    // A node that neither consumes nor produces anything from others.
    let orphan = make_node_doc("orphan", &[], &[]);
    let docs = make_plugin_docs("pkg", vec![orphan]);
    insert_test_plugin(&plugin_registry, "pkg", &docs);
    node_registry
        .register_from_docs("pkg", &docs)
        .unwrap();

    let graph_registry = GraphRegistry::from_registries(&plugin_registry, &node_registry);
    let net = graph_registry.net();

    // The orphan node should still be listed.
    assert!(net.nodes.iter().any(|n| n.node_fqn == "pkg::orphan"));
    assert!(net
        .nodes
        .iter()
        .find(|n| n.node_fqn == "pkg::orphan")
        .unwrap()
        .consumes
        .is_empty());
    assert!(net
        .nodes
        .iter()
        .find(|n| n.node_fqn == "pkg::orphan")
        .unwrap()
        .produces
        .is_empty());

    // No edges should reference the orphan node.
    let orphan_refs = net
        .edges
        .iter()
        .filter(|e| e.from == "pkg::orphan" || e.to == "pkg::orphan")
        .count();
    assert_eq!(orphan_refs, 0);
}

#[test]
fn edge_deduplication_keeps_single_edge() {
    let plugin_registry = PluginRegistry::default();
    let mut node_registry = NodeRegistry::default();

    // Producer produces two fields that the consumer both consumes.
    // If they share a label name they should be deduplicated.
    let producer = make_node_doc("producer", &[], &["value", "extra"]);
    let consumer = make_node_doc("consumer", &["value", "value"], &[]);

    let docs_p = make_plugin_docs("pkg/p", vec![producer]);
    let docs_c = make_plugin_docs("pkg/c", vec![consumer]);
    insert_test_plugin(&plugin_registry, "pkg/p", &docs_p);
    insert_test_plugin(&plugin_registry, "pkg/c", &docs_c);
    node_registry
        .register_from_docs("pkg/p", &docs_p)
        .unwrap();
    node_registry
        .register_from_docs("pkg/c", &docs_c)
        .unwrap();

    let graph_registry = GraphRegistry::from_registries(&plugin_registry, &node_registry);
    let net = graph_registry.net();

    // Edge from producer to consumer for label "value" should appear exactly once.
    let matching: Vec<_> = net
        .edges
        .iter()
        .filter(|e| {
            e.from == "pkg/p::producer"
                && e.to == "pkg/c::consumer"
                && e.label.as_deref() == Some("value")
        })
        .collect();
    // dedup_by should have collapsed duplicates.
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one deduplicated edge, got {}",
        matching.len()
    );
}
