use cordis_runtime::plugin::tooling::{refresh_artifact_index, sync_plugin_docs};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .expect("fixtures must exist")
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

#[test]
fn sync_plugin_docs_rewrites_dylib_interfaces_json() {
    let temp = setup_fixture_copy();
    let expr_docs = temp.path().join("plugins/expr/docs/agent/interfaces.json");
    fs::write(&expr_docs, "{}\n").expect("write broken docs");

    let written = sync_plugin_docs(temp.path()).expect("sync docs should succeed");
    assert!(written.iter().any(|path| path == &expr_docs));

    let content = fs::read_to_string(&expr_docs).expect("read synced docs");
    let value: Value = serde_json::from_str(&content).expect("valid synced json");
    assert_eq!(value.get("plugin_path").and_then(|v| v.as_str()), Some("expr"));
    assert_eq!(
        value.get("command_name").and_then(|v| v.as_str()),
        Some("Expr")
    );
}

#[test]
fn refresh_artifact_index_recomputes_hashes() {
    let temp = setup_fixture_copy();
    let index_path = temp.path().join("artifacts/index.json");
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&index_path).expect("read index"))
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
    fs::write(&index_path, serde_json::to_string_pretty(&value).expect("serialize index"))
        .expect("write broken index");

    let refreshed = refresh_artifact_index(temp.path()).expect("refresh index should succeed");
    let (_, shell_hash) = refreshed
        .into_iter()
        .find(|(plugin_path, _)| plugin_path == "shell")
        .expect("shell hash refreshed");

    let updated: Value = serde_json::from_str(&fs::read_to_string(&index_path).expect("read updated index"))
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
