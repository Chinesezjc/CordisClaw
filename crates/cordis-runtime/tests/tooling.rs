use cordis_runtime::plugin::package::PackageResolver;
use cordis_runtime::plugin::tooling::{refresh_artifact_index, sync_plugin_docs};
use serde_json::Value;
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
    copy_dir_all(&fixtures_root(), temp.path());
    temp
}

fn append_expr_evaluator_child(root: &Path, child_name: &str) {
    let manifest_path = root.join("plugins/expr/evaluator/Cargo.toml");
    let content = fs::read_to_string(&manifest_path).expect("read evaluator manifest");
    let needle = "]\n\n[package.metadata.cordis.abi_fingerprint]";
    let replacement = format!(
        "  {{ source = \"./{child_name}\", required = true, grants = [] }},\n]\n\n[package.metadata.cordis.abi_fingerprint]"
    );
    let patched = content.replacen(needle, &replacement, 1);
    assert_ne!(
        patched, content,
        "evaluator manifest should gain child entry"
    );
    fs::write(&manifest_path, patched).expect("write evaluator manifest");
}

fn write_expr_mod_child_without_generated_docs(root: &Path) {
    let plugin_dir = root.join("plugins/expr/evaluator/mod");
    fs::create_dir_all(plugin_dir.join("src")).expect("mkdir mod src");
    fs::create_dir_all(plugin_dir.join("tests")).expect("mkdir mod tests");
    fs::create_dir_all(plugin_dir.join("docs/human")).expect("mkdir mod docs human");

    fs::write(
        plugin_dir.join("Cargo.toml"),
        r#"[package]
name = "expr_evaluator_mod"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["rlib", "dylib"]

[package.metadata.cordis]
plugin_path = "expr/evaluator/mod"
abi_kind = "rust"
declared_nodes = ["expr_mod"]
children = []

[package.metadata.cordis.abi_fingerprint]
rustc_version = "1.85.1"
target_triple = "x86_64-unknown-linux-gnu"
crate_hash = "crate_expr_mod_v1"
api_hash = "api_v2"

[dependencies]
cordis-plugin-sdk = { path = "../../../../../crates/cordis-plugin-sdk" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[workspace]
"#,
    )
    .expect("write mod manifest");
    fs::write(
        plugin_dir.join("src/core.rs"),
        r#"#[derive(Debug, Default, Clone, Copy)]
pub struct ModPlugin;

impl ModPlugin {
    pub fn apply(&self, lhs: f64, rhs: f64) -> f64 {
        lhs % rhs
    }
}

pub fn apply(lhs: f64, rhs: f64) -> f64 {
    ModPlugin.apply(lhs, rhs)
}
"#,
    )
    .expect("write mod core");
    fs::write(
        plugin_dir.join("src/lib.rs"),
        r#"mod core;

pub use core::*;

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BinaryOpRequest {
    lhs: f64,
    rhs: f64,
}

#[derive(Debug, Serialize)]
struct BinaryOpResponse {
    value: f64,
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "expr_evaluator_mod",
        "expr/evaluator/mod",
        "0.1.0",
        None,
        vec![node_doc(
            "expr_mod",
            "Compute lhs modulo rhs.",
            json!({
                "type": "object",
                "required": ["lhs", "rhs"],
                "properties": {
                    "lhs": { "type": "number" },
                    "rhs": { "type": "number" }
                }
            }),
            json!({
                "type": "object",
                "properties": { "value": { "type": "number" } }
            }),
            &[],
            &["division_by_zero"],
        )],
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_expr_mod_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    let response = match serde_json::from_str::<BinaryOpRequest>(&req.payload) {
        Ok(request) => BinaryOpResponse {
            value: apply(request.lhs, request.rhs),
        },
        Err(_) => BinaryOpResponse { value: f64::NAN },
    };
    json_response(&response)
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
"#,
    )
    .expect("write mod lib");
    fs::write(
        plugin_dir.join("tests/mod.rs"),
        r#"use expr_evaluator_mod::apply;

#[test]
fn modulo_returns_remainder() {
    assert_eq!(apply(5.0, 2.0), 1.0);
}
"#,
    )
    .expect("write mod tests");
    fs::write(
        plugin_dir.join("docs/human/overview.md"),
        "# Expr Mod\n\nSibling child plugin for modulo evaluation.\n",
    )
    .expect("write mod human docs");
}

#[test]
fn sync_plugin_docs_rewrites_dylib_interfaces_json() {
    let temp = setup_fixture_copy();
    let expr_docs = temp.path().join("plugins/expr/docs/agent/interfaces.json");
    fs::write(&expr_docs, "{}\n").expect("write broken docs");

    let written = sync_plugin_docs(temp.path()).expect("sync docs should succeed");
    assert!(written.iter().any(|path| path == &expr_docs));

    let content = fs::read_to_string(&expr_docs).expect("read synced docs");
    let value: Value = serde_json::from_str(&content).expect("valid synced json");
    assert_eq!(
        value.get("plugin_path").and_then(|v| v.as_str()),
        Some("expr")
    );
    assert_eq!(
        value.get("command_name").and_then(|v| v.as_str()),
        Some("Expr")
    );
}

#[test]
fn refresh_artifact_index_recomputes_hashes() {
    let temp = setup_fixture_copy();
    let index_path = temp.path().join("artifacts/index.json");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&index_path).expect("read index"))
            .expect("parse index");

    let entries = value
        .get_mut("entries")
        .and_then(|v| v.as_array_mut())
        .expect("entries array");
    let shell = entries
        .iter_mut()
        .find(|entry| entry.get("plugin_path").and_then(|v| v.as_str()) == Some("shell"))
        .expect("shell entry");
    shell["sha256"] = Value::String("deadbeef".to_string());
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&value).expect("serialize index"),
    )
    .expect("write broken index");

    let refreshed = refresh_artifact_index(temp.path()).expect("refresh index should succeed");
    let (_, shell_hash) = refreshed
        .into_iter()
        .find(|(plugin_path, _)| plugin_path == "shell")
        .expect("shell hash refreshed");

    let updated: Value =
        serde_json::from_str(&fs::read_to_string(&index_path).expect("read updated index"))
            .expect("parse updated index");
    let updated_hash = updated
        .get("entries")
        .and_then(|v| v.as_array())
        .and_then(|entries| {
            entries.iter().find_map(|entry| {
                (entry.get("plugin_path").and_then(|v| v.as_str()) == Some("shell"))
                    .then(|| entry.get("sha256").and_then(|v| v.as_str()))
                    .flatten()
            })
        })
        .expect("updated shell hash");

    assert_eq!(updated_hash, shell_hash);
    assert_ne!(updated_hash, "deadbeef");
}

#[test]
fn package_resolver_allows_new_dylib_child_without_generated_agent_docs() {
    let temp = setup_fixture_copy();
    append_expr_evaluator_child(temp.path(), "mod");
    write_expr_mod_child_without_generated_docs(temp.path());

    let graph = PackageResolver::new(temp.path().join("plugins"))
        .resolve()
        .expect("resolver should allow generated docs for new dylib child");
    let plugin = graph
        .plugins
        .get("expr/evaluator/mod")
        .expect("new child plugin should be discovered");
    assert_eq!(plugin.docs.plugin_path, "expr/evaluator/mod");
    assert!(
        plugin.docs.nodes.is_empty(),
        "missing generated docs should synthesize a placeholder until rebuild writes real docs"
    );
}
