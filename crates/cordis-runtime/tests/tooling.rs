use cordis_plugin_sdk::{node_doc, plugin_docs, pretty_json, AbiFingerprint};
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::{
    ArtifactIndex, ArtifactIndexEntry, ArtifactKind, InputProbe, PluginArtifact,
    ARTIFACT_INDEX_SCHEMA_VERSION,
};
use cordis_runtime::plugin::package::PackageResolver;
use cordis_runtime::plugin::tooling::{
    ensure_fixture_artifacts, prepare_artifacts, read_plugin_docs, rebuild_fixture_artifacts,
    rebuild_plugin_workspace, refresh_artifact_index, sync_plugin_docs, PrepareMode,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
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

/// `sync_plugin_docs` maps a failed `create_dir_all(docs_dir)` to Io: a
/// read-only `plugins/` directory blocks creating the per-plugin
/// `docs/agent/` path. Covers the create-docs-dir map_err arm.
#[cfg(unix)]
#[test]
fn sync_plugin_docs_errors_when_docs_dir_cannot_be_created() {
    use std::os::unix::fs::PermissionsExt;
    let temp = synthetic_fixtures(vec![json_entry("alpha", "alpha.json", "aa")]);
    let plugins = temp.path().join("plugins");
    // Deny writes under plugins/ so create_dir_all(plugins/alpha/docs/agent) fails.
    fs::set_permissions(&plugins, fs::Permissions::from_mode(0o555)).unwrap();
    let err = sync_plugin_docs(temp.path());
    fs::set_permissions(&plugins, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(err, Err(RuntimeError::Io { .. })), "err: {err:?}");
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

// ---------------------------------------------------------------------------
// Build-orchestration chain — drives `prepare_artifacts` /
// `rebuild_fixture_artifacts` / `rebuild_plugin_workspace` against a minimal,
// freshly-built dylib plugin workspace under a TempDir. These exercise the
// private orchestration surface (prepare_artifacts_locked, build_plugin_contexts,
// DependencySnapshot::load / load_workspace_metadata / collect_local_dependency_dirs,
// build_dirty_dylib_plugins, materialize_artifact_entry, inspect_dylib_contract,
// build_plugin_artifact, built_dylib_path) end-to-end.
//
// They require a working `cargo` toolchain and compile a real dylib against the
// in-repo `cordis-plugin-sdk`, so the produced artifact matches the host arch
// (dlopen-able). The compile is bounded to a single tiny crate; CARGO_TARGET_DIR
// is redirected into the TempDir so cold/warm builds stay isolated and cheap.
// ---------------------------------------------------------------------------

/// Absolute path to the in-repo plugin SDK crate, used as the `path`
/// dependency for the synthetic plugins so they link the same ABI symbol
/// table the loader expects.
fn sdk_crate_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../cordis-plugin-sdk")
        .canonicalize()
        .expect("plugin sdk crate must exist")
}

/// Write a minimal but *complete* dylib plugin crate at `dir`:
/// - `Cargo.toml` (dylib crate-type, cordis metadata, path dep on the SDK)
/// - `src/lib.rs` exporting the v2 ABI (real `abi_fingerprint` / `docs` / `handle`)
/// - scaffold dirs (`tests/`, `docs/human/overview.md`) the resolver requires
/// - `docs/agent/interfaces.json` matching the exported docs
///
/// `abi_fingerprint` uses `AbiFingerprint::current_build`, so the recorded
/// fingerprint matches the built dylib's runtime fingerprint (materialize's
/// AbiMismatch guard passes). `extra_cordis` injects e.g. a `children = [...]`
/// line or a nested `[workspace]` marker.
fn write_dylib_plugin(dir: &Path, crate_name: &str, plugin_path: &str, extra_cordis: &str) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::create_dir_all(dir.join("docs/human")).unwrap();
    fs::create_dir_all(dir.join("docs/agent")).unwrap();

    let sdk = sdk_crate_dir();
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["rlib", "dylib"]

[package.metadata.cordis]
plugin_path = "{plugin_path}"
abi_kind = "rust"
declared_nodes = ["nd"]
{extra_cordis}

[package.metadata.cordis.abi_fingerprint]
crate_hash = "crate_{crate_name}_v1"
api_hash = "api_v2"

[dependencies]
cordis-plugin-sdk = {{ path = "{sdk}" }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
"#,
            sdk = sdk.display(),
        ),
    )
    .unwrap();

    fs::write(
        dir.join("src/lib.rs"),
        format!(
            r#"use cordis_plugin_sdk::{{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginDocs,
    PluginRequest, PluginResponse,
}};
use serde_json::json;

fn docs_value() -> PluginDocs {{
    plugin_docs(
        "{crate_name}",
        "{plugin_path}",
        "0.1.0",
        Some("Cmd"),
        vec![node_doc(
            "nd",
            "demo node",
            json!({{"type": "object"}}),
            json!({{"type": "object"}}),
            &[],
            &[],
        )],
        None,
    )
}}

fn abi() -> AbiFingerprint {{
    AbiFingerprint::current_build("crate_{crate_name}_v1", "api_v2")
}}

fn handle(_req: PluginRequest) -> PluginResponse {{
    json_response(&json!({{"ok": true}}))
}}

export_plugin_api! {{
    abi_fingerprint = abi(),
    docs = docs_value(),
    handle = handle,
}}
"#,
        ),
    )
    .unwrap();

    fs::write(dir.join("tests/smoke.rs"), "fn main() {}\n").unwrap();
    fs::write(
        dir.join("docs/human/overview.md"),
        format!("# {plugin_path}\n"),
    )
    .unwrap();

    // interfaces.json must match the dylib's runtime `docs_value()` byte-for-byte:
    // materialize_artifact_entry rewrites this file from the runtime docs, and
    // it is itself a build input. If it drifts, the next incremental pass sees a
    // changed fingerprint and rebuilds. Mirroring the exported docs (same node)
    // keeps the rewrite a no-op so `reuse` is observable.
    let docs = plugin_docs(
        crate_name,
        plugin_path,
        "0.1.0",
        Some("Cmd"),
        vec![node_doc(
            "nd",
            "demo node",
            serde_json::json!({"type": "object"}),
            serde_json::json!({"type": "object"}),
            &[],
            &[],
        )],
        None,
    );
    fs::write(dir.join("docs/agent/interfaces.json"), pretty_json(&docs)).unwrap();
}

/// Build a synthetic fixtures root whose `plugins/` is a single-crate cargo
/// workspace containing one dylib plugin. Returns the TempDir; the fixtures
/// root is `temp.path()`.
fn dylib_plugin_fixtures(crate_name: &str, plugin_path: &str) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let plugins = temp.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("Cargo.toml"),
        format!("[workspace]\nmembers = [\"{crate_name}\"]\nresolver = \"2\"\n"),
    )
    .unwrap();
    write_dylib_plugin(
        &plugins.join(crate_name),
        crate_name,
        plugin_path,
        "children = []",
    );
    temp
}

/// Redirect cargo's target dir to `<fixtures>/plugins/target` — the TempDir's
/// own workspace target. This keeps builds self-contained per test *and* matches
/// `rebuild_plugin_workspace`'s hardcoded `plugins/target/debug` source path, so
/// the metadata-driven (`prepare_artifacts`) and hardcoded (`rebuild_plugin_workspace`)
/// paths both resolve the freshly built dylib.
fn scoped_target_dir(temp: &TempDir) -> PathBuf {
    let dir = temp.path().join("plugins/target");
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Guard that sets `CARGO_TARGET_DIR` for the duration of a build and restores
/// the previous value on drop. `prepare_artifacts` shells out to `cargo build`
/// without an explicit target dir, so we steer it via the env var.
struct TargetDirGuard {
    previous: Option<std::ffi::OsString>,
}

impl TargetDirGuard {
    fn set(path: &Path) -> Self {
        let previous = std::env::var_os("CARGO_TARGET_DIR");
        std::env::set_var("CARGO_TARGET_DIR", path);
        Self { previous }
    }
}

impl Drop for TargetDirGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("CARGO_TARGET_DIR", value),
            None => std::env::remove_var("CARGO_TARGET_DIR"),
        }
    }
}

fn read_index(fixtures_root: &Path) -> Value {
    let index_path = fixtures_root.join("artifacts/index.json");
    serde_json::from_str(&fs::read_to_string(&index_path).expect("read index"))
        .expect("parse index")
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_full_builds_dylib_and_writes_index_entry() {
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    // Full mode: no prior index -> build_plugin_contexts + DependencySnapshot::load
    // (cargo metadata) + build_dirty_dylib_plugins (cargo build workspace member)
    // + materialize_artifact_entry (stage-then-rename + inspect_dylib_contract).
    let report =
        prepare_artifacts(temp.path(), PrepareMode::Full).expect("full prepare should succeed");
    assert!(report.full_rebuild, "full mode reports full_rebuild");
    assert_eq!(
        report.rebuilt.len(),
        1,
        "the single dylib plugin should be (re)built, got {:?}",
        report.rebuilt
    );
    assert_eq!(report.rebuilt[0].0, "demo");
    assert!(report.reused.is_empty(), "nothing to reuse on a cold build");

    // The staged artifact exists and is a real dylib the loader can dlopen.
    let artifact = temp
        .path()
        .join("artifacts")
        .join(format!("demo.{}", std::env::consts::DLL_EXTENSION));
    assert!(artifact.exists(), "dylib artifact should be staged");

    // Index entry records the dylib kind, a 64-char sha256, and the runtime docs.
    let index = read_index(temp.path());
    let entries = index.get("entries").and_then(|v| v.as_array()).unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["plugin_path"], "demo");
    assert_eq!(entry["artifact_kind"], "dylib");
    assert_eq!(
        entry["sha256"].as_str().map(str::len),
        Some(64),
        "sha256 hashed from the freshly staged dylib"
    );
    // materialize_artifact_entry wrote the runtime docs back to the plugin.
    let written_docs = temp.path().join("plugins/demo/docs/agent/interfaces.json");
    let docs: Value = serde_json::from_str(&fs::read_to_string(&written_docs).unwrap()).unwrap();
    assert_eq!(docs["plugin_path"], "demo");
    assert_eq!(docs["command_name"], "Cmd");

    // read_plugin_docs over the real dylib exercises inspect's sibling path
    // (LoadedDylibApi::open + runtime docs parse).
    let runtime_docs = read_plugin_docs(&artifact).expect("read docs from dylib");
    assert_eq!(runtime_docs.plugin_path, "demo");
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_incremental_reuses_clean_entry() {
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    // First pass builds; second incremental pass finds nothing dirty and
    // reuses the existing entry (compute_dirty_state -> false branch).
    prepare_artifacts(temp.path(), PrepareMode::Full).expect("initial build");
    let report = prepare_artifacts(temp.path(), PrepareMode::Incremental)
        .expect("incremental prepare should succeed");
    assert!(!report.full_rebuild);
    assert!(
        report.rebuilt.is_empty(),
        "clean incremental pass rebuilds nothing, got {:?}",
        report.rebuilt
    );
    assert_eq!(report.reused, vec!["demo".to_string()]);
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_incremental_rebuilds_after_source_edit() {
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    prepare_artifacts(temp.path(), PrepareMode::Full).expect("initial build");

    // Touch the plugin source so its input probe + fingerprint drift, forcing
    // compute_dirty_state -> true and a rebuild via build_dirty_dylib_plugins.
    let lib = temp.path().join("plugins/demo/src/lib.rs");
    let mut src = fs::read_to_string(&lib).unwrap();
    src.push_str("\n// touch to invalidate fingerprint\n");
    fs::write(&lib, src).unwrap();

    let report = prepare_artifacts(temp.path(), PrepareMode::Incremental)
        .expect("incremental prepare after edit");
    assert_eq!(
        report.rebuilt.len(),
        1,
        "edited plugin should rebuild, got {:?}",
        report.rebuilt
    );
    assert_eq!(report.rebuilt[0].0, "demo");
}

#[test]
#[serial_test::serial]
fn rebuild_fixture_artifacts_returns_built_plugin() {
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    // Thin wrapper over prepare_artifacts(Full) that returns only the rebuilt list.
    let rebuilt = rebuild_fixture_artifacts(temp.path()).expect("rebuild fixtures");
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].0, "demo");
    assert_eq!(rebuilt[0].1.len(), 64, "second tuple field is the sha256");
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_full_requires_repo_sources() {
    // Full mode against a directory without plugins/Cargo.toml must error
    // (can_prepare_fixture_artifacts == false + Full -> Invariant).
    let temp = TempDir::new().unwrap();
    let err = prepare_artifacts(temp.path(), PrepareMode::Full);
    assert!(matches!(err, Err(RuntimeError::Invariant { .. })));
}

#[test]
fn prepare_artifacts_incremental_noop_without_repo_sources() {
    // Incremental mode is a documented no-op when there's no plugins workspace.
    let temp = TempDir::new().unwrap();
    let report = prepare_artifacts(temp.path(), PrepareMode::Incremental)
        .expect("incremental noop should succeed");
    assert!(report.rebuilt.is_empty());
    assert!(report.reused.is_empty());
    assert!(!report.full_rebuild);
}

#[test]
fn ensure_fixture_artifacts_is_false_without_repo_sources() {
    // `ensure_fixture_artifacts` is a thin wrapper over
    // prepare_artifacts(Incremental) returning whether anything was rebuilt.
    // Against a workspace-less directory it is a no-op: nothing rebuilt.
    let temp = TempDir::new().unwrap();
    let rebuilt = ensure_fixture_artifacts(temp.path()).expect("ensure should succeed");
    assert!(!rebuilt, "no repo sources -> nothing rebuilt");
}

#[test]
#[serial_test::serial]
fn ensure_fixture_artifacts_true_on_cold_dylib_build() {
    // A cold fixtures root with dylib sources: the incremental pass has no
    // prior index, so it performs a full build and reports rebuilt == true.
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);
    let rebuilt = ensure_fixture_artifacts(temp.path()).expect("cold ensure builds");
    assert!(rebuilt, "cold build must report a rebuild");
}

#[test]
#[serial_test::serial]
fn rebuild_plugin_workspace_named_plugin_builds_and_refreshes_index() {
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    // Seed a full index first so the named-rebuild path has an index to refresh.
    prepare_artifacts(temp.path(), PrepareMode::Full).expect("seed index");

    // Corrupt the recorded sha256 so we can prove rebuild_plugin_workspace
    // refreshes it after staging the freshly built dylib.
    let index_path = temp.path().join("artifacts/index.json");
    let mut index: Value = serde_json::from_str(&fs::read_to_string(&index_path).unwrap()).unwrap();
    index["entries"][0]["sha256"] = Value::String("deadbeef".to_string());
    fs::write(&index_path, serde_json::to_string_pretty(&index).unwrap()).unwrap();

    // "/demo" -> build just the `demo` package, stage lib{demo}.dylib into
    // artifacts/, and refresh index.json's sha256.
    let built = rebuild_plugin_workspace(temp.path(), "/demo").expect("named rebuild");
    assert_eq!(built.len(), 1);
    assert_eq!(built[0].0, "demo");
    assert!(
        built[0].1.contains("->"),
        "detail records src -> dst, got {}",
        built[0].1
    );

    let updated = read_index(temp.path());
    let sha = updated["entries"][0]["sha256"].as_str().unwrap();
    assert_ne!(sha, "deadbeef", "index sha256 should be refreshed");
    assert_eq!(sha.len(), 64);
}

#[test]
#[serial_test::serial]
fn rebuild_plugin_workspace_root_slash_rebuilds_everything() {
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    // "/" delegates to rebuild_fixture_artifacts (full rebuild of all plugins).
    let rebuilt = rebuild_plugin_workspace(temp.path(), "/").expect("root rebuild");
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].0, "demo");
}

#[test]
#[serial_test::serial]
fn rebuild_plugin_workspace_unknown_plugin_errors() {
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    // A package name cargo doesn't know about -> `cargo build -p ...` fails,
    // surfaced as InvalidArgument.
    let err = rebuild_plugin_workspace(temp.path(), "/no_such_plugin");
    assert!(matches!(err, Err(RuntimeError::InvalidArgument { .. })));
}

/// Fixtures with a workspace-member parent dylib plugin and a *non*-member
/// child dylib plugin (the child carries its own `[workspace]` marker, so
/// `DependencySnapshot::is_workspace_member` returns false for it). This routes
/// the child through the `build_plugin_artifact` + `built_dylib_path` branch
/// of `build_dirty_dylib_plugins` / `materialize_artifact_entry`, which the
/// workspace-member fast path never touches.
fn parent_child_fixtures() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let plugins = temp.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("Cargo.toml"),
        "[workspace]\nmembers = [\"parent\"]\nexclude = [\"parent/child\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    // Parent is a workspace member; declares the nested child as required.
    write_dylib_plugin(
        &plugins.join("parent"),
        "parent",
        "parent",
        "children = [{ source = \"./child\", required = true, grants = [] }]",
    );
    // Child crate name must equal normalize_crate_name("parent/child").
    // Its own `[workspace]` line makes it a standalone workspace, so the parent
    // metadata excludes it and prepare_artifacts builds it via its own manifest.
    write_dylib_plugin(
        &plugins.join("parent/child"),
        "parent_child",
        "parent/child",
        "children = []\n[workspace]",
    );
    temp
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_builds_non_workspace_member_child_via_own_manifest() {
    let temp = parent_child_fixtures();
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    let report =
        prepare_artifacts(temp.path(), PrepareMode::Full).expect("full prepare with nested child");

    // Both plugins built: parent through the workspace path, child through
    // build_plugin_artifact + built_dylib_path (its own manifest metadata).
    let mut built: Vec<String> = report.rebuilt.iter().map(|(p, _)| p.clone()).collect();
    built.sort();
    assert_eq!(
        built,
        vec!["parent".to_string(), "parent/child".to_string()]
    );

    // Both dylibs staged, both index entries dylib-kind with real hashes.
    let ext = std::env::consts::DLL_EXTENSION;
    assert!(temp.path().join(format!("artifacts/parent.{ext}")).exists());
    assert!(temp
        .path()
        .join(format!("artifacts/parent_child.{ext}"))
        .exists());

    let index = read_index(temp.path());
    let entries = index.get("entries").and_then(|v| v.as_array()).unwrap();
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert_eq!(entry["artifact_kind"], "dylib");
        assert_eq!(entry["sha256"].as_str().map(str::len), Some(64));
    }
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_detects_abi_fingerprint_mismatch() {
    // The manifest's declared crate_hash and the dylib's runtime crate_hash
    // (baked in by `abi()`) must agree. Rewrite the manifest so its declared
    // crate_hash diverges from what the built dylib exports; materialize's
    // AbiMismatch guard should reject it.
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    let manifest = temp.path().join("plugins/demo/Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    let patched = text.replace(
        "crate_hash = \"crate_demo_v1\"",
        "crate_hash = \"crate_demo_DECLARED_MISMATCH\"",
    );
    assert_ne!(
        patched, text,
        "manifest crate_hash line should be rewritten"
    );
    fs::write(&manifest, patched).unwrap();

    let err = prepare_artifacts(temp.path(), PrepareMode::Full);
    assert!(
        matches!(&err, Err(RuntimeError::AbiMismatch { plugin_path, .. }) if plugin_path == "demo"),
        "expected AbiMismatch, got {err:?}"
    );
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_detects_docs_plugin_path_mismatch() {
    // materialize_artifact_entry compares the dylib's *runtime* docs.plugin_path
    // (baked into `docs_value()` at compile time) against the resolved
    // plugin.plugin_path (from the manifest). Rewrite only the source's
    // `docs_value()` plugin_path so the compiled dylib reports a different
    // plugin_path than the manifest declares. The manifest + interfaces.json
    // still agree (resolution passes) and the ABI fingerprint is unchanged
    // (AbiMismatch does NOT fire), so the DocsContract guard is what rejects it.
    let temp = dylib_plugin_fixtures("demo", "demo");
    let target = scoped_target_dir(&temp);
    let _guard = TargetDirGuard::set(&target);

    let lib = temp.path().join("plugins/demo/src/lib.rs");
    let text = fs::read_to_string(&lib).unwrap();
    // The plugin_path is the second positional arg to `plugin_docs`, uniquely
    // identified by its adjacency to the version literal below it.
    let patched = text.replace(
        "        \"demo\",\n        \"0.1.0\",",
        "        \"demo_runtime_diverged\",\n        \"0.1.0\",",
    );
    assert_ne!(
        patched, text,
        "docs_value plugin_path arg should be rewritten"
    );
    fs::write(&lib, patched).unwrap();

    let err = prepare_artifacts(temp.path(), PrepareMode::Full);
    match err {
        Err(RuntimeError::DocsContract {
            plugin_path,
            message,
        }) => {
            assert_eq!(plugin_path, "demo");
            assert!(
                message.contains("demo_runtime_diverged"),
                "message should name the divergent runtime path: {message}"
            );
        }
        other => panic!("expected DocsContract, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// JSON-artifact plugin path. A plugin whose crate is *not* a dylib
// (`crate-type` omits dylib/cdylib) is materialized through the
// `materialize_artifact_entry` JSON branch: no `cargo build`, no dylib
// inspection — the resolved docs + build-spec exports/execution are written
// straight into a `<plugin>.json` artifact. `DependencySnapshot::load` still
// runs `cargo metadata` on the workspace, so this needs a working toolchain,
// but it compiles nothing.
// ---------------------------------------------------------------------------

/// Write a minimal JSON-artifact plugin crate (no dylib crate-type) with the
/// full scaffold `PackageResolver` requires: `src/lib.rs`, `tests/`,
/// `docs/human/overview.md`, and a matching `docs/agent/interfaces.json`.
fn write_json_plugin(dir: &Path, crate_name: &str, plugin_path: &str) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::create_dir_all(dir.join("docs/human")).unwrap();
    fs::create_dir_all(dir.join("docs/agent")).unwrap();

    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"

[package.metadata.cordis]
plugin_path = "{plugin_path}"
abi_kind = "rust"
declared_nodes = ["nd"]
children = []

[package.metadata.cordis.abi_fingerprint]
crate_hash = "crate_{crate_name}_v1"
api_hash = "api_v2"

[package.metadata.cordis.artifact]
exports = ["svc.demo"]
"#,
        ),
    )
    .unwrap();
    fs::write(dir.join("src/lib.rs"), "// json plugin, no dylib\n").unwrap();
    fs::write(dir.join("tests/smoke.rs"), "fn main() {}\n").unwrap();
    fs::write(
        dir.join("docs/human/overview.md"),
        format!("# {plugin_path}\n"),
    )
    .unwrap();

    let docs = plugin_docs(
        plugin_path,
        plugin_path,
        "0.1.0",
        Some("Cmd"),
        vec![node_doc(
            "nd",
            "demo node",
            serde_json::json!({"type": "object"}),
            serde_json::json!({"type": "object"}),
            &[],
            &[],
        )],
        None,
    );
    fs::write(dir.join("docs/agent/interfaces.json"), pretty_json(&docs)).unwrap();
}

/// Fixtures root whose `plugins/` workspace holds a single JSON-artifact
/// plugin. The workspace manifest lists it as a member so `cargo metadata`
/// resolves it.
fn json_plugin_fixtures(crate_name: &str, plugin_path: &str) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let plugins = temp.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("Cargo.toml"),
        format!("[workspace]\nmembers = [\"{crate_name}\"]\nresolver = \"2\"\n"),
    )
    .unwrap();
    write_json_plugin(&plugins.join(crate_name), crate_name, plugin_path);
    temp
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_full_writes_json_artifact_without_building() {
    // A JSON-only plugin exercises materialize_artifact_entry's non-dylib
    // branch: PluginArtifact is assembled from the resolved docs +
    // exports/execution and written to `<plugin>.json`; no cargo build runs
    // and no dylib is inspected.
    let temp = json_plugin_fixtures("jsonp", "jsonp");

    let report =
        prepare_artifacts(temp.path(), PrepareMode::Full).expect("full prepare of JSON plugin");
    assert!(report.full_rebuild);
    assert_eq!(report.rebuilt.len(), 1);
    assert_eq!(report.rebuilt[0].0, "jsonp");
    assert_eq!(report.rebuilt[0].1.len(), 64, "sha256 of the JSON artifact");

    // The staged artifact is a JSON file the loader parses (not a dylib).
    let artifact = temp.path().join("artifacts/jsonp.json");
    assert!(artifact.exists(), "JSON artifact should be written");
    let value: Value = serde_json::from_str(&fs::read_to_string(&artifact).unwrap()).unwrap();
    assert_eq!(value["plugin_path"], "jsonp");
    // Exports declared in the manifest flow through the build spec into the
    // artifact.
    assert_eq!(value["exports"][0], "svc.demo");

    // The index entry records the JSON kind.
    let index = read_index(temp.path());
    let entries = index.get("entries").and_then(|v| v.as_array()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["artifact_kind"], "json");
    assert_eq!(entries[0]["sha256"].as_str().map(str::len), Some(64));

    // read_plugin_docs over the JSON artifact returns the resolved docs.
    let docs = read_plugin_docs(&artifact).expect("read JSON artifact docs");
    assert_eq!(docs.plugin_path, "jsonp");
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_incremental_reuses_clean_json_entry() {
    // Second incremental pass over an unchanged JSON plugin finds nothing
    // dirty and reuses the entry (compute_dirty_state -> false), driving the
    // reuse branch of prepare_artifacts_locked.
    let temp = json_plugin_fixtures("jsonp", "jsonp");
    prepare_artifacts(temp.path(), PrepareMode::Full).expect("seed JSON index");

    let report =
        prepare_artifacts(temp.path(), PrepareMode::Incremental).expect("incremental JSON prepare");
    assert!(!report.full_rebuild);
    assert!(report.rebuilt.is_empty(), "clean pass rebuilds nothing");
    assert_eq!(report.reused, vec!["jsonp".to_string()]);
}

#[test]
#[serial_test::serial]
fn prepare_artifacts_second_full_pass_clears_existing_artifacts_dir() {
    // A second Full pass finds the `artifacts/` directory already populated
    // from the first build, so prepare_artifacts_locked takes the
    // `full_rebuild && artifacts_dir.exists()` branch and removes it before
    // rebuilding. A stray file planted in `artifacts/` must not survive.
    let temp = json_plugin_fixtures("jsonp", "jsonp");
    prepare_artifacts(temp.path(), PrepareMode::Full).expect("first full build");

    let stray = temp.path().join("artifacts/stray.txt");
    fs::write(&stray, b"leftover").unwrap();
    assert!(stray.exists());

    let report = prepare_artifacts(temp.path(), PrepareMode::Full).expect("second full build");
    assert!(report.full_rebuild);
    assert_eq!(report.rebuilt.len(), 1);
    assert!(
        !stray.exists(),
        "second full pass should clear the existing artifacts dir"
    );
    assert!(temp.path().join("artifacts/jsonp.json").exists());
}
