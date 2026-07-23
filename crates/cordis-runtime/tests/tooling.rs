use cordis_plugin_sdk::{plugin_docs, pretty_json, AbiFingerprint};
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::{
    ArtifactIndex, ArtifactIndexEntry, ArtifactKind, InputProbe, PluginArtifact,
    ARTIFACT_INDEX_SCHEMA_VERSION,
};
use cordis_runtime::plugin::package::PackageResolver;
use cordis_runtime::plugin::tooling::{read_plugin_docs, refresh_artifact_index, sync_plugin_docs};
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

mod support;

use support::fixtures_root;

// ---------------------------------------------------------------------------
// Synthetic JSON-only workspace helpers. These exercise the public
// tooling surface (sync_plugin_docs / refresh_artifact_index /
// read_plugin_docs) without any dylib or `cargo` build, so they run on
// every host regardless of the fixture artifacts' target triple.
// ---------------------------------------------------------------------------

fn sample_abi() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "rustc-test".to_string(),
        target_triple: "test-triple".to_string(),
        crate_hash: "crate_v1".to_string(),
        api_hash: "api_v1".to_string(),
    }
}

fn json_entry(plugin_path: &str, artifact_rel: &str, sha: &str) -> ArtifactIndexEntry {
    ArtifactIndexEntry {
        plugin_path: plugin_path.to_string(),
        version: "0.1.0".to_string(),
        abi_fingerprint: sample_abi(),
        artifact_path: artifact_rel.to_string(),
        sha256: sha.to_string(),
        built_at: "0".to_string(),
        parent: None,
        required: true,
        grants_from_parent: Vec::new(),
        docs: plugin_docs(
            plugin_path,
            plugin_path,
            "0.1.0",
            Some("Cmd"),
            Vec::new(),
            None,
        ),
        exports: Vec::new(),
        execution: None,
        artifact_kind: ArtifactKind::Json,
        build_fingerprint: "fp".to_string(),
        input_probe: InputProbe::default(),
        local_path_deps: Vec::new(),
    }
}

/// Build a minimal fixtures root: `plugins/Cargo.toml` (workspace marker
/// so sync_plugin_docs accepts it) + `artifacts/index.json` referencing
/// JSON artifacts written under `artifacts/`.
fn synthetic_fixtures(entries: Vec<ArtifactIndexEntry>) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    fs::create_dir_all(root.join("plugins")).unwrap();
    fs::write(root.join("plugins/Cargo.toml"), "[workspace]\n").unwrap();
    let artifacts_dir = root.join("artifacts");
    fs::create_dir_all(&artifacts_dir).unwrap();

    for entry in &entries {
        // Materialise the JSON artifact the entry points at so sha256_file
        // can hash it during refresh.
        let artifact = PluginArtifact {
            plugin_path: entry.plugin_path.clone(),
            abi_fingerprint: entry.abi_fingerprint.clone(),
            docs: entry.docs.clone(),
            exports: Vec::new(),
            execution: None,
        };
        let path = artifacts_dir.join(&entry.artifact_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, pretty_json(&artifact)).unwrap();
    }

    let index = ArtifactIndex {
        schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
        generated_at: "0".to_string(),
        topo_order: entries.iter().map(|e| e.plugin_path.clone()).collect(),
        entries,
    };
    fs::write(artifacts_dir.join("index.json"), pretty_json(&index)).unwrap();
    temp
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
# P1-48: dylib alone no longer bypasses the generated-docs scaffold check;
# the resolver requires this explicit opt-in (paired with crate-type dylib).
allow_generated_docs = true

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

#[allow(dead_code)]
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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
    if !support::linux_dylib_artifacts_available() {
        eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
        return;
    }
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

// ---------------------------------------------------------------------------
// sync_plugin_docs — JSON-only, cross-platform.
// ---------------------------------------------------------------------------

#[test]
fn sync_plugin_docs_writes_interfaces_for_json_entries() {
    let temp = synthetic_fixtures(vec![
        json_entry("alpha", "alpha.json", "aa"),
        json_entry("beta/child", "beta_child.json", "bb"),
    ]);
    let written = sync_plugin_docs(temp.path()).expect("sync docs ok");
    assert_eq!(written.len(), 2);

    // Nested plugin path resolves under plugins/<segments>/docs/agent/.
    let alpha_docs = temp.path().join("plugins/alpha/docs/agent/interfaces.json");
    let beta_docs = temp
        .path()
        .join("plugins/beta/child/docs/agent/interfaces.json");
    assert!(alpha_docs.exists());
    assert!(beta_docs.exists());
    assert!(written.contains(&alpha_docs));
    assert!(written.contains(&beta_docs));

    let value: Value = serde_json::from_str(&fs::read_to_string(&alpha_docs).unwrap()).unwrap();
    assert_eq!(
        value.get("plugin_path").and_then(|v| v.as_str()),
        Some("alpha")
    );
    assert_eq!(
        value.get("command_name").and_then(|v| v.as_str()),
        Some("Cmd")
    );
}

#[test]
fn sync_plugin_docs_overwrites_drifted_interfaces_json() {
    let temp = synthetic_fixtures(vec![json_entry("alpha", "alpha.json", "aa")]);
    let docs_path = temp.path().join("plugins/alpha/docs/agent/interfaces.json");
    fs::create_dir_all(docs_path.parent().unwrap()).unwrap();
    fs::write(&docs_path, "{\"plugin_path\":\"WRONG\"}\n").unwrap();

    sync_plugin_docs(temp.path()).expect("sync docs ok");
    let value: Value = serde_json::from_str(&fs::read_to_string(&docs_path).unwrap()).unwrap();
    assert_eq!(
        value.get("plugin_path").and_then(|v| v.as_str()),
        Some("alpha"),
        "drifted docs should be rewritten from the index entry"
    );
}

#[test]
fn sync_plugin_docs_rejects_missing_plugins_workspace() {
    let temp = TempDir::new().unwrap();
    // No plugins/Cargo.toml -> Invariant error.
    let err = sync_plugin_docs(temp.path());
    assert!(matches!(err, Err(RuntimeError::Invariant { .. })));
}

#[test]
fn sync_plugin_docs_errors_when_index_missing() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("plugins")).unwrap();
    fs::write(temp.path().join("plugins/Cargo.toml"), "[workspace]\n").unwrap();
    // plugins workspace exists but artifacts/index.json does not.
    let err = sync_plugin_docs(temp.path());
    assert!(matches!(err, Err(RuntimeError::Io { .. })));
}

// ---------------------------------------------------------------------------
// refresh_artifact_index — JSON-only, cross-platform.
// ---------------------------------------------------------------------------

#[test]
fn refresh_artifact_index_rewrites_all_hashes() {
    let temp = synthetic_fixtures(vec![
        json_entry("alpha", "alpha.json", "deadbeef"),
        json_entry("beta", "beta.json", "cafef00d"),
    ]);
    let index_path = temp.path().join("artifacts/index.json");

    let refreshed = refresh_artifact_index(temp.path()).expect("refresh ok");
    assert_eq!(refreshed.len(), 2);
    // Returned hashes are real 64-char sha256 hex, not the placeholder.
    for (_, hash) in &refreshed {
        assert_eq!(hash.len(), 64);
        assert_ne!(hash, "deadbeef");
    }

    // On-disk index is updated to match.
    let updated: Value = serde_json::from_str(&fs::read_to_string(&index_path).unwrap()).unwrap();
    let entries = updated.get("entries").and_then(|v| v.as_array()).unwrap();
    for entry in entries {
        let sha = entry.get("sha256").and_then(|v| v.as_str()).unwrap();
        assert_eq!(sha.len(), 64);
    }
}

#[test]
fn refresh_artifact_index_errors_when_artifact_file_missing() {
    let temp = synthetic_fixtures(vec![json_entry("alpha", "alpha.json", "aa")]);
    // Delete the referenced artifact so sha256_file fails.
    fs::remove_file(temp.path().join("artifacts/alpha.json")).unwrap();
    let err = refresh_artifact_index(temp.path());
    assert!(matches!(err, Err(RuntimeError::Io { .. })));
}

#[test]
fn refresh_artifact_index_errors_on_bad_index_json() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("artifacts")).unwrap();
    fs::write(temp.path().join("artifacts/index.json"), "{ broken").unwrap();
    let err = refresh_artifact_index(temp.path());
    assert!(matches!(err, Err(RuntimeError::ArtifactIndexParse { .. })));
}

// ---------------------------------------------------------------------------
// read_plugin_docs — JSON artifact path, cross-platform.
// ---------------------------------------------------------------------------

#[test]
fn read_plugin_docs_from_json_artifact_roundtrips() {
    let temp = synthetic_fixtures(vec![json_entry("alpha", "alpha.json", "aa")]);
    let artifact = temp.path().join("artifacts/alpha.json");
    let docs = read_plugin_docs(&artifact).expect("read docs ok");
    assert_eq!(docs.plugin_path, "alpha");
    assert_eq!(docs.command_name.as_deref(), Some("Cmd"));
}
