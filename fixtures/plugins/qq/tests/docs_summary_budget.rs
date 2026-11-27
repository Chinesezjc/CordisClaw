//! The kernel refuses node summaries above its size cap, so an over-long
//! summary is silently DROPPED from the generated `docs/agent/interfaces.json`
//! — the rebuild then fails with "docs mismatch: expected 15 nodes, got 14"
//! while `docs_value()` still declares every node.
//!
//! Every other plugin keeps its summaries under ~400 characters. This test
//! reads the SHIPPED artifact (what the kernel actually reads), not the Rust
//! literal, and names any summary that would be dropped.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;

/// Longest node summary the kernel keeps; anything longer disappears.
const SUMMARY_CHAR_CAP: usize = 400;

fn interfaces_json() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/agent/interfaces.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The node list, accepting either `{"nodes": [...]}` or a bare array.
fn nodes(json: &Value) -> Vec<Value> {
    if let Some(nodes) = json.get("nodes").and_then(Value::as_array) {
        return nodes.clone();
    }
    if let Some(nodes) = json.as_array() {
        return nodes.clone();
    }
    let keys: Vec<&str> = json
        .as_object()
        .map(|object| object.keys().map(String::as_str).collect())
        .unwrap_or_default();
    panic!("no node list in docs/agent/interfaces.json (top-level keys: {keys:?})")
}

fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
}

/// The node ids `[package.metadata.cordis] declared_nodes` lists.
fn declared_node_ids() -> Vec<String> {
    let path = manifest_path();
    let manifest =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let start = manifest
        .find("declared_nodes")
        .expect("declared_nodes in Cargo.toml");
    let rest = &manifest[start..];
    let open = rest.find('[').expect("declared_nodes list");
    let close = rest[open..].find(']').expect("declared_nodes list closes");
    rest[open + 1..open + close]
        .split(',')
        .map(|entry| entry.trim().trim_matches('"').trim().to_string())
        .filter(|id| !id.is_empty())
        .collect()
}

/// The shipped artifact is what the kernel reads on reload, so it must mention
/// every node `declared_nodes` promises — otherwise the reload aborts with
/// `docs mismatch: expected N nodes, got M`.
#[test]
fn the_artifact_documents_every_declared_node() {
    let declared = declared_node_ids();
    assert!(!declared.is_empty(), "Cargo.toml declares no nodes");
    let json = interfaces_json();
    let documented: Vec<String> = nodes(&json)
        .iter()
        .filter_map(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let missing: Vec<&String> = declared
        .iter()
        .filter(|id| !documented.contains(id))
        .collect();
    assert!(
        missing.is_empty(),
        "declared in Cargo.toml but absent from docs/agent/interfaces.json \
         (the reload aborts with a docs mismatch): {missing:?}"
    );
    let undeclared: Vec<&String> = documented
        .iter()
        .filter(|id| !declared.contains(id))
        .collect();
    assert!(
        undeclared.is_empty(),
        "documented but not declared in Cargo.toml: {undeclared:?}"
    );
    assert_eq!(
        documented.len(),
        declared.len(),
        "expected {} nodes, got {}: {documented:?}",
        declared.len(),
        documented.len()
    );
}

#[test]
fn every_node_summary_fits_the_kernels_size_cap() {
    let json = interfaces_json();
    let nodes = nodes(&json);
    assert!(
        !nodes.is_empty(),
        "the artifact must document the plugin's nodes"
    );

    let mut offenders: Vec<String> = Vec::new();
    for node in &nodes {
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<missing id>");
        let summary = node.get("summary").and_then(Value::as_str).unwrap_or("");
        let len = summary.chars().count();
        if len > SUMMARY_CHAR_CAP {
            offenders.push(format!("{id}: {len} chars"));
        }
    }
    assert!(
        offenders.is_empty(),
        "the kernel drops node summaries above {SUMMARY_CHAR_CAP} chars, so these nodes \
         vanish from the artifact (docs mismatch on every rebuild): {offenders:?}"
    );
}
