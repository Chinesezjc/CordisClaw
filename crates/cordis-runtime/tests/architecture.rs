use cordis_runtime::context::ContextRegistry;
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::{NodeOutcome, PluginLoadResult, PluginUnavailableReason};
use cordis_runtime::plugin::invoke::PluginInvoker;
use cordis_runtime::execution::scheduler::{run_deterministic, ScheduledNode, SchedulerConfig};
use cordis_runtime::plugin::loader::{default_loader_config, Loader};
use cordis_runtime::plugin::tooling::{prepare_artifacts, PrepareMode};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
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
        output.plugin_registry.get("root/child").unwrap().load_result,
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
    assert_eq!(route_value.get("id").and_then(|x| x.as_str()), Some("child_entry"));

    let allowed = output
        .context
        .inject::<String>("root/child", "service.db")
        .expect("service.db should be granted");
    assert!(allowed.contains("service:root:service.db"));

    let denied = output.context.inject::<String>("root/child", "service.cache");
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

    assert!(
        plugins
            .iter()
            .any(|plugin| plugin.get("plugin_path").and_then(|v| v.as_str()) == Some("expr"))
    );
    assert!(
        plugins
            .iter()
            .any(|plugin| plugin.get("plugin_path").and_then(|v| v.as_str()) == Some("shell"))
    );
    assert!(
        nodes.iter().any(|node| {
            node.get("node_fqn").and_then(|v| v.as_str()) == Some("expr::expr_entry")
        })
    );

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
fn registered_dag_json_and_html_are_available() {
    let temp = setup_fixture_copy();
    let config = default_loader_config(temp.path());
    let loader = Loader::new(config);
    let output = loader.load().expect("load should pass");

    let dag_json = output
        .graph_registry
        .handle_get_json("/graphs/registered-dag")
        .expect("dag json should exist");
    let nodes = dag_json
        .get("nodes")
        .and_then(|value| value.as_array())
        .expect("nodes array");
    let edges = dag_json
        .get("edges")
        .and_then(|value| value.as_array())
        .expect("edges array");

    assert!(
        nodes.iter().any(|node| {
            node.get("node_fqn").and_then(|v| v.as_str()) == Some("expr/lexer::expr_lexer")
        }),
        "lexer node should appear in dag"
    );
    assert!(
        nodes.iter().any(|node| {
            node.get("node_fqn").and_then(|v| v.as_str()) == Some("expr/parser::expr_parser")
        }),
        "parser node should appear in dag"
    );
    assert!(
        nodes.iter().any(|node| {
            node.get("node_fqn").and_then(|v| v.as_str()) == Some("expr/evaluator::expr_evaluator")
        }),
        "evaluator node should appear in dag"
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
        .handle_get_html("/graphs/registered-dag.html")
        .expect("dag html should exist");
    assert!(html.contains("<!doctype html>"));
    assert!(html.contains("Registered DAG"));
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
    let evaluator_value: Value =
        serde_json::from_str(&evaluator.payload).expect("evaluator json");
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
    let output = loader.load().expect("optional child failure should not abort");

    assert!(matches!(
        output.plugin_registry.get("root").unwrap().load_result,
        PluginLoadResult::Loaded
    ));
    assert!(matches!(
        output.plugin_registry.get("root/child").unwrap().load_result,
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
    let output = loader.load().expect("loader should continue with unavailable state");

    assert!(matches!(
        output.plugin_registry.get("root/child").unwrap().load_result,
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
    let output = loader.load().expect("loader should continue with unavailable state");

    assert!(matches!(
        output.plugin_registry.get("root/child").unwrap().load_result,
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
    let output = loader.load().expect("loader should continue with unavailable state");

    assert!(matches!(
        output
            .context
            .inject::<String>("root/child", "service.db"),
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
            SchedulerConfig { max_parallelism: 1 },
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
