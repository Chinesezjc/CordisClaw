use super::{
    artifact_paths_to_backup, atomic_write_bytes, build_execution_net, build_execution_payload,
    build_snapshot_with_staged_root, cleanup_stale_snapshot_dirs, collect_context_files_recursive,
    collect_plugin_context_paths, default_snapshot_root, determine_root_plugin_path,
    extract_warning_blocks, fill_missing_execution_traces, insert_context_file_if_exists,
    make_snapshot_dir_name, normalize_request_id, normalize_warning_source_path,
    plugin_change_reasons, plugin_context_priority, plugin_path_from_runtime_error,
    render_child_plugin_core, render_child_plugin_lib, render_child_plugin_manifest,
    render_child_plugin_overview, render_child_plugin_test, sanitize_child_plugin_segment,
    select_registered_net_subgraph, should_track_context_file, validated_verification_command,
    workspace_manifest_lock, ExecutionInvocationTrace,
};
use crate::core::error::RuntimeError;
use crate::core::models::NodeOutcome;
use crate::core::models::PluginUnavailableReason;
use crate::execution::engine::{
    ExecutionMetrics, ExecutionOutput, ExecutionTransitionKind, TriggerInput,
};
use crate::execution::net::{CorrelationKey, Token, TokenMeta};
use crate::service::graph_registry::{
    RegisteredNet, RegisteredNetEdge, RegisteredNetEdgeKind, RegisteredNetNode,
};
use cordis_plugin_sdk::NodeType;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

// ── validated_verification_command: default-alias short-circuit (5525) ──
#[test]
fn validated_verification_command_returns_fallback_for_bare_alias() {
    // explicit "check" with required_prefix "cargo check" and a fallback:
    // the alias arm returns the fallback verbatim.
    let out = validated_verification_command(
        Some("check".to_string()),
        Some("cargo check --workspace".to_string()),
        "cargo check",
    )
    .expect("bare alias with fallback resolves to fallback");
    assert_eq!(out, "cargo check --workspace");

    // "test" alias against "cargo test".
    let out = validated_verification_command(
        Some("test".to_string()),
        Some("cargo test --all".to_string()),
        "cargo test",
    )
    .expect("test alias with fallback resolves to fallback");
    assert_eq!(out, "cargo test --all");
}

#[test]
fn validated_verification_command_trims_and_accepts_prefixed() {
    let out = validated_verification_command(
        Some("  cargo check --lib  ".to_string()),
        None,
        "cargo check",
    )
    .expect("prefixed command accepted");
    assert_eq!(out, "cargo check --lib");
}

// ── should_track_context_file (5571) ────────────────────────────────
#[test]
fn should_track_context_file_matches_source_extensions() {
    assert!(should_track_context_file("plugins/foo/src/lib.rs"));
    assert!(should_track_context_file("plugins/foo/Cargo.toml"));
    assert!(should_track_context_file(
        "plugins/foo/docs/agent/interfaces.json"
    ));
    assert!(should_track_context_file(
        "plugins/foo/docs/human/overview.md"
    ));
    // Unknown extension → not tracked.
    assert!(!should_track_context_file("plugins/foo/artifacts/foo.so"));
    assert!(!should_track_context_file("plugins/foo/README"));
}

// ── plugin_context_priority: the else/fallback bucket (5600) ─────────
#[test]
fn plugin_context_priority_orders_by_bucket() {
    assert_eq!(plugin_context_priority("a/Cargo.toml").0, 0);
    assert_eq!(plugin_context_priority("a/src/core.rs").0, 1);
    assert_eq!(plugin_context_priority("a/src/lib.rs").0, 2);
    assert_eq!(plugin_context_priority("a/tests/it.rs").0, 3);
    assert_eq!(plugin_context_priority("a/docs/human/x.md").0, 4);
    assert_eq!(plugin_context_priority("a/docs/agent/x.json").0, 5);
    assert_eq!(plugin_context_priority("a/src/other.rs").0, 6);
    // Anything else → the catch-all bucket 7.
    assert_eq!(plugin_context_priority("a/notes.txt").0, 7);
}

// ── sanitize_child_plugin_segment (5612, 5621) ──────────────────────
#[test]
fn sanitize_child_plugin_segment_normalizes_and_defaults() {
    // Non-alphanumeric runs collapse to single dashes and trim.
    assert_eq!(sanitize_child_plugin_segment("  Foo Bar!! "), "foo-bar");
    assert_eq!(sanitize_child_plugin_segment("//Multiply//"), "multiply");
    // All-punctuation input normalizes to empty → "child" default.
    assert_eq!(sanitize_child_plugin_segment("***"), "child");
    assert_eq!(sanitize_child_plugin_segment(""), "child");
}

// ── render_child_plugin_* (5632-5683) ───────────────────────────────
#[test]
fn render_child_plugin_manifest_embeds_names_and_paths() {
    let manifest = render_child_plugin_manifest("my-child", "root/child", "child_entry");
    // crate name has dashes replaced with underscores in [package].name.
    assert!(manifest.contains("name = \"my_child\""));
    assert!(manifest.contains("plugin_path = \"root/child\""));
    assert!(manifest.contains("declared_nodes = [\"child_entry\"]"));
    assert!(manifest.contains("crate_hash = \"crate_my_child_v1\""));
    assert!(manifest.contains("allow_generated_docs = true"));
}

#[test]
fn render_child_plugin_lib_escapes_summary_quotes() {
    let lib = render_child_plugin_lib("my-child", "root/child", "child_entry", "a \"quoted\" sum");
    assert!(lib.contains("\"child_entry\""));
    assert!(lib.contains("crate_my-child_v1"));
    // Embedded quotes in the summary are backslash-escaped.
    assert!(lib.contains("a \\\"quoted\\\" sum"));
}

#[test]
fn render_child_plugin_core_builds_pascal_type_name() {
    let core = render_child_plugin_core("binary-multiply");
    // "binary-multiply" → PascalCase "BinaryMultiply".
    assert!(core.contains("pub enum BinaryMultiplyError"));
    assert!(core.contains("pub struct BinaryMultiplyPlugin"));
    assert!(core.contains("NotImplemented"));
}

#[test]
fn render_child_plugin_test_and_overview_embed_identifiers() {
    let test = render_child_plugin_test("my_child");
    assert!(test.contains("use my_child::apply;"));
    assert!(test.contains("scaffold_exports_apply"));

    let overview = render_child_plugin_overview("root/child");
    assert!(overview.starts_with("# root/child\n"));
    assert!(overview.contains("Cordis plugin-iteration agent"));
}

// ── extract_warning_blocks: block boundary flush (5838-5840) ─────────
#[test]
fn extract_warning_blocks_flushes_on_boundary_line() {
    let text = "\
warning: unused variable `x`
  --> src/lib.rs:1:5
Compiling next-crate v0.1.0
warning: unused import
  --> src/lib.rs:2:5";
    let blocks = extract_warning_blocks(text);
    // The "Compiling " boundary flushes the first block; a trailing block
    // is pushed at EOF.
    assert_eq!(blocks.len(), 2);
    assert!(blocks[0].starts_with("warning: unused variable `x`"));
    assert!(!blocks[0].contains("Compiling"));
    assert!(blocks[1].starts_with("warning: unused import"));
}

// ── select_registered_net_subgraph: multi-hop upstream (5915) ────────
#[test]
fn select_registered_net_subgraph_visits_each_upstream_once() {
    // Diamond: a→b, a→c, b→d, c→d. Target d must pull a,b,c,d exactly once.
    let edge = |from: &str, to: &str| RegisteredNetEdge {
        from: from.to_string(),
        to: to.to_string(),
        kind: RegisteredNetEdgeKind::Data,
        label: None,
    };
    let net = RegisteredNet {
        nodes: Vec::new(),
        edges: vec![
            edge("a", "b"),
            edge("a", "c"),
            edge("b", "d"),
            edge("c", "d"),
        ],
        diagnostics: Vec::new(),
    };
    let selected = select_registered_net_subgraph(&net, "d");
    let mut got = selected.into_iter().collect::<Vec<_>>();
    got.sort();
    assert_eq!(got, vec!["a", "b", "c", "d"]);
}

// ── build_execution_net (5934-6010) ─────────────────────────────────
fn net_node(fqn: &str, node_type: NodeType, topo: usize) -> RegisteredNetNode {
    RegisteredNetNode {
        node_fqn: fqn.to_string(),
        plugin_path: fqn.split("::").next().unwrap_or(fqn).to_string(),
        node_id: fqn.split("::").nth(1).unwrap_or(fqn).to_string(),
        consumes: Vec::new(),
        produces: Vec::new(),
        topo_level: topo,
        node_type,
    }
}

fn registered_node(fqn: &str, node_type: NodeType) -> crate::plugin::registry::RegisteredNode {
    crate::plugin::registry::RegisteredNode {
        node_fqn: fqn.to_string(),
        plugin_path: fqn.split("::").next().unwrap_or(fqn).to_string(),
        node_id: fqn.split("::").nth(1).unwrap_or(fqn).to_string(),
        node_type,
    }
}

#[test]
fn build_execution_net_empty_selection_yields_terminal_fallback() {
    // No net node matches the selected set → single Terminal transition
    // keyed on the target fqn (5940-5956).
    let net = RegisteredNet::default();
    let selected = BTreeSet::from(["missing::node".to_string()]);
    let fallback = registered_node("missing::node", NodeType::Terminal);
    let spec = build_execution_net(&net, &selected, "missing::node", &fallback);
    assert!(spec.places.is_empty());
    assert!(spec.arcs.is_empty());
    assert_eq!(spec.transitions.len(), 1);
    let t = &spec.transitions[0];
    assert_eq!(t.transition.transition_id, "missing::node");
    assert!(matches!(t.kind, ExecutionTransitionKind::Terminal));
    assert_eq!(t.logical_group.as_deref(), Some("execute"));
    assert!(t.node_type.is_none());
}

#[test]
fn build_execution_net_maps_all_node_types_and_join_policy() {
    // Nodes cover every NodeType arm (5978-5986). Edge a→b makes b's
    // incoming count 1 (AllOf); the source a has incoming 0 (AnyOf).
    let net = RegisteredNet {
        nodes: vec![
            net_node("p::a", NodeType::Task, 0),
            net_node("p::b", NodeType::Router, 1),
            net_node("p::c", NodeType::Gate, 1),
            net_node("p::d", NodeType::Terminal, 2),
        ],
        edges: vec![RegisteredNetEdge {
            from: "p::a".to_string(),
            to: "p::b".to_string(),
            kind: RegisteredNetEdgeKind::Data,
            label: Some("payload".to_string()),
        }],
        diagnostics: Vec::new(),
    };
    let selected = BTreeSet::from([
        "p::a".to_string(),
        "p::b".to_string(),
        "p::c".to_string(),
        "p::d".to_string(),
    ]);
    // Target is selected → no fallback terminal appended.
    let fallback = registered_node("p::a", NodeType::Task);
    let spec = build_execution_net(&net, &selected, "p::a", &fallback);
    assert_eq!(spec.transitions.len(), 4);
    let by_id = |id: &str| {
        spec.transitions
            .iter()
            .find(|t| t.transition.transition_id == id)
            .expect("transition present")
    };
    assert!(matches!(by_id("p::a").kind, ExecutionTransitionKind::Task));
    assert!(matches!(
        by_id("p::b").kind,
        ExecutionTransitionKind::Router { .. }
    ));
    assert!(matches!(
        by_id("p::c").kind,
        ExecutionTransitionKind::Gate { .. }
    ));
    assert!(matches!(
        by_id("p::d").kind,
        ExecutionTransitionKind::Terminal
    ));
    // a has no incoming selected edge → AnyOf; b has one → AllOf.
    use crate::execution::net::JoinPolicy;
    assert!(matches!(
        by_id("p::a").transition.join_policy,
        JoinPolicy::AnyOf
    ));
    assert!(matches!(
        by_id("p::b").transition.join_policy,
        JoinPolicy::AllOf
    ));
}

#[test]
fn build_execution_net_appends_fallback_terminal_when_target_unselected() {
    // Selected nodes exist, but the requested target fqn is NOT among them
    // → the extra fallback Terminal transition is appended (5998-6010).
    let net = RegisteredNet {
        nodes: vec![net_node("p::a", NodeType::Task, 0)],
        edges: Vec::new(),
        diagnostics: Vec::new(),
    };
    let selected = BTreeSet::from(["p::a".to_string()]);
    let fallback = registered_node("p::target", NodeType::Terminal);
    let spec = build_execution_net(&net, &selected, "p::target", &fallback);
    assert_eq!(spec.transitions.len(), 2);
    let fallback_t = spec
        .transitions
        .iter()
        .find(|t| t.transition.transition_id == "p::target")
        .expect("fallback terminal appended");
    assert!(matches!(fallback_t.kind, ExecutionTransitionKind::Terminal));
    assert!(fallback_t.node_type.is_none());
}

// ── build_execution_payload (6077-6084) ─────────────────────────────
fn token_with_payload(payload: Value) -> Token {
    Token {
        key: CorrelationKey::new(""),
        payload,
        meta: TokenMeta {
            execution_id: "e".to_string(),
            transition_id: "t".to_string(),
            logical_group: "g".to_string(),
            sequence: 0,
            outcome: NodeOutcome::Success,
        },
    }
}

#[test]
fn build_execution_payload_merges_labeled_inputs_only() {
    let mut base = Map::new();
    base.insert("keep".to_string(), json!(1));
    let inputs = vec![
        // Labeled input whose payload has the field → merged in.
        TriggerInput {
            place_id: "pl1".to_string(),
            label: Some("added".to_string()),
            token: token_with_payload(json!({ "added": 42, "extra": 9 })),
        },
        // No label → skipped (6078-6080).
        TriggerInput {
            place_id: "pl2".to_string(),
            label: None,
            token: token_with_payload(json!({ "ignored": 1 })),
        },
        // Labeled but field absent from payload → skipped (6081-6083).
        TriggerInput {
            place_id: "pl3".to_string(),
            label: Some("absent".to_string()),
            token: token_with_payload(json!({ "other": 1 })),
        },
    ];
    let out = build_execution_payload(&base, &inputs);
    assert_eq!(out.get("keep"), Some(&json!(1)));
    assert_eq!(out.get("added"), Some(&json!(42)));
    // Only the named field is copied, not sibling fields from the token.
    assert!(out.get("extra").is_none());
    assert!(out.get("absent").is_none());
}

// ── fill_missing_execution_traces (6126-6134) ───────────────────────
#[test]
fn fill_missing_execution_traces_inserts_placeholder_and_sets_outcome() {
    let mut outcomes = BTreeMap::new();
    outcomes.insert("p::new".to_string(), NodeOutcome::Failure);
    outcomes.insert("p::existing".to_string(), NodeOutcome::Success);
    let output = ExecutionOutput {
        execution_id: "e".to_string(),
        order: Vec::new(),
        outcomes,
        keyed_outcomes: BTreeMap::new(),
        metrics: ExecutionMetrics::default(),
    };
    let mut traces = BTreeMap::new();
    // Pre-seed one trace with existing data to prove only the outcome is set.
    traces.insert(
        "p::existing".to_string(),
        ExecutionInvocationTrace {
            node_fqn: "p::existing".to_string(),
            plugin_path: "p".to_string(),
            node_id: "existing".to_string(),
            attempt: 3,
            outcome: None,
            request_payload: Some(json!({"r": 1})),
            response_payload: None,
            error: None,
        },
    );
    fill_missing_execution_traces(&output, &mut traces);
    // New fqn gets a default-shaped placeholder trace.
    let new_trace = traces.get("p::new").expect("placeholder inserted");
    assert_eq!(new_trace.node_fqn, "p::new");
    assert_eq!(new_trace.plugin_path, "");
    assert_eq!(new_trace.attempt, 0);
    assert_eq!(new_trace.outcome, Some(NodeOutcome::Failure));
    // Existing trace keeps its fields but gains the outcome.
    let existing = traces.get("p::existing").expect("existing preserved");
    assert_eq!(existing.attempt, 3);
    assert_eq!(existing.request_payload, Some(json!({"r": 1})));
    assert_eq!(existing.outcome, Some(NodeOutcome::Success));
}

// ── plugin_path_from_runtime_error: every plugin-bearing variant + None
//    (6387-6408) ─────────────────────────────────────────────────────
#[test]
fn plugin_path_from_runtime_error_covers_all_variants() {
    use crate::core::models::AbiFingerprint;
    use std::path::PathBuf;
    let pp = |s: &str| Some(s.to_string());

    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::InvalidChildSource {
            parent: "root".to_string(),
            child_source: "c".to_string(),
            reason: "r".to_string(),
        }),
        pp("root")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::DuplicatePluginPath {
            plugin_path: "dup".to_string(),
            first: PathBuf::from("/a"),
            second: PathBuf::from("/b"),
        }),
        pp("dup")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::MissingScaffold {
            plugin_path: "ms".to_string(),
            missing: vec!["x".to_string()],
        }),
        pp("ms")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::DocsContract {
            plugin_path: "dc".to_string(),
            message: "m".to_string(),
        }),
        pp("dc")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::ArtifactIndexMissing {
            plugin_path: "aim".to_string(),
        }),
        pp("aim")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::ArtifactFileMissing {
            plugin_path: "afm".to_string(),
            artifact_path: PathBuf::from("/a.so"),
        }),
        pp("afm")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::ArtifactHashMismatch {
            plugin_path: "ahm".to_string(),
            expected: "e".to_string(),
            actual: "a".to_string(),
        }),
        pp("ahm")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::AbiMismatch {
            plugin_path: "abi".to_string(),
            expected: Box::new(AbiFingerprint::current_build("c", "a")),
            actual: Box::new(AbiFingerprint::current_build("c2", "a2")),
            fingerprint_diff: Vec::new(),
        }),
        pp("abi")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::PluginUnavailable {
            plugin_path: "pu".to_string(),
            reason: PluginUnavailableReason::InitFailed,
            required: true,
        }),
        pp("pu")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::PluginExecutionUnsupported {
            plugin_path: "peu".to_string(),
            artifact_path: PathBuf::from("/a"),
        }),
        pp("peu")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::PluginInvocationFailed {
            plugin_path: "pif".to_string(),
            message: "m".to_string(),
        }),
        pp("pif")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::PluginDocsNotFound {
            plugin_path: "pdnf".to_string(),
        }),
        pp("pdnf")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::NodeDocsNotFound {
            plugin_path: "ndnf".to_string(),
            node_id: "n".to_string(),
        }),
        pp("ndnf")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::PermissionDenied {
            plugin_path: "pd".to_string(),
            service: "s".to_string(),
        }),
        pp("pd")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::ContextPluginUnavailable {
            plugin_path: "cpu".to_string(),
        }),
        pp("cpu")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::ServiceNotFound {
            plugin_path: "snf".to_string(),
            service: "s".to_string(),
        }),
        pp("snf")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::ServiceTypeMismatch {
            plugin_path: "stm".to_string(),
            service: "s".to_string(),
        }),
        pp("stm")
    );
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::DuplicateService {
            plugin_path: "ds".to_string(),
            service: "s".to_string(),
        }),
        pp("ds")
    );
}

// ── determine_root_plugin_path: empty-target error (6417) ────────────
fn empty_snapshot() -> super::RuntimeSnapshot {
    super::runtime_snapshot_from_output(
        crate::plugin::loader::LoadOutput {
            execution_id: "snap-empty".to_string(),
            plugin_registry: crate::plugin::registry::PluginRegistry::default(),
            node_registry: crate::plugin::registry::NodeRegistry::default(),
            doc_registry: crate::service::doc_registry::DocRegistry::default(),
            graph_registry: crate::service::graph_registry::GraphRegistry::default(),
            context: crate::context::RuntimeContext::default(),
            metrics: crate::plugin::loader::LoaderMetrics::default(),
        },
        std::path::PathBuf::from("/tmp/cordis-empty-staged"),
        BTreeMap::new(),
    )
}

#[test]
fn determine_root_plugin_path_empty_targets_errors_byte_exact() {
    // The empty target list errors before the snapshot registry is
    // ever consulted.
    let snapshot = empty_snapshot();
    let err = determine_root_plugin_path(&snapshot, &[]).expect_err("empty target list must error");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message }
                if message == "plugin iteration requires target_plugin_paths or an observed issue"),
        "unexpected: {err:?}"
    );
}

#[test]
fn determine_root_plugin_path_errors_when_no_shared_loaded_root() {
    // Two targets share a common prefix ("root") but that prefix is not in
    // the (empty) plugin registry, so all candidates pop off and the final
    // "do not share a loaded subtree root" error fires (6430-6449).
    let snapshot = empty_snapshot();
    let err = determine_root_plugin_path(&snapshot, &["root/a".to_string(), "root/b".to_string()])
        .expect_err("no loaded root must error");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message }
                if message == "target plugin paths do not share a loaded subtree root: root/a, root/b"),
        "unexpected: {err:?}"
    );
}

// ── normalize_request_id (6628-6629) ────────────────────────────────
#[test]
fn normalize_request_id_keeps_nonblank_and_generates_for_blank() {
    assert_eq!(
        normalize_request_id(Some("given-id".to_string()), "pfx"),
        "given-id"
    );
    // Blank / whitespace / None all fall to the generated branch.
    let gen1 = normalize_request_id(Some("   ".to_string()), "pfx");
    assert!(gen1.starts_with("pfx-"));
    let gen2 = normalize_request_id(None, "pfx");
    assert!(gen2.starts_with("pfx-"));
    // The process-local counter keeps successive generated ids distinct.
    assert_ne!(gen1, gen2);
}

// ── artifact_paths_to_backup (6832-6839) ────────────────────────────
#[test]
fn artifact_paths_to_backup_handles_empty_missing_and_present() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fixtures = temp.path();
    // Empty/whitespace-only path → nothing.
    assert!(artifact_paths_to_backup(fixtures, "/").is_empty());
    // Non-empty path but no artifact file on disk → nothing.
    assert!(artifact_paths_to_backup(fixtures, "root/child").is_empty());
    // Now create artifacts/root/child.so → returned as (rel, abs).
    let so = fixtures.join("artifacts/root/child.so");
    std::fs::create_dir_all(so.parent().unwrap()).unwrap();
    std::fs::write(&so, b"bytes").unwrap();
    let out = artifact_paths_to_backup(fixtures, "root/child");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, "artifacts/root/child.so");
    assert_eq!(out[0].1, so);
}

// ── default_snapshot_root: canonicalize + hash layout (6977) ─────────
#[test]
fn default_snapshot_root_is_deterministic_under_temp_dir() {
    let temp = tempfile::tempdir().expect("tempdir");
    let a = default_snapshot_root(temp.path());
    let b = default_snapshot_root(temp.path());
    assert_eq!(a, b, "same fixtures root hashes to the same snapshot root");
    assert!(a.starts_with(std::env::temp_dir().join("cordis-runtime-host")));
}

// ── collect_context_files_recursive + insert_context_file_if_exists +
//    collect_plugin_context_paths (filesystem seams,
//    6463-6528, 6575, 6611-6621) ─────────────────────────────────────
#[test]
fn collect_context_files_recursive_walks_tracked_extensions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let src = root.join("plugins/foo/src");
    std::fs::create_dir_all(src.join("nested")).unwrap();
    std::fs::write(src.join("lib.rs"), b"// rs").unwrap();
    std::fs::write(src.join("nested/core.rs"), b"// rs").unwrap();
    // Non-tracked extension is skipped.
    std::fs::write(src.join("blob.bin"), b"bin").unwrap();
    let mut files = BTreeSet::new();
    collect_context_files_recursive(root, &src, &mut files).expect("walk ok");
    assert!(files.contains("plugins/foo/src/lib.rs"));
    assert!(files.contains("plugins/foo/src/nested/core.rs"));
    assert!(!files.iter().any(|p| p.ends_with("blob.bin")));
}

#[test]
fn insert_context_file_if_exists_only_inserts_present_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    std::fs::create_dir_all(root.join("plugins/foo")).unwrap();
    std::fs::write(root.join("plugins/foo/Cargo.toml"), b"[package]").unwrap();
    let mut files = BTreeSet::new();
    insert_context_file_if_exists(root, "plugins/foo/Cargo.toml", &mut files);
    insert_context_file_if_exists(root, "plugins/foo/missing.rs", &mut files);
    assert!(files.contains("plugins/foo/Cargo.toml"));
    assert_eq!(files.len(), 1);
}

#[test]
fn collect_plugin_context_paths_errors_when_no_files_discovered() {
    let temp = tempfile::tempdir().expect("tempdir");
    // No plugins/ tree at all → all_files empty → InvalidArgument.
    let err = collect_plugin_context_paths(temp.path(), "root", &["root".to_string()])
        .expect_err("no discovered files must error");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message }
                if message == "no planner context files discovered for plugin iteration"),
        "unexpected: {err:?}"
    );
}

#[test]
fn collect_plugin_context_paths_gathers_all_and_focus_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let base = root.join("plugins/root");
    std::fs::create_dir_all(base.join("src")).unwrap();
    std::fs::create_dir_all(base.join("tests")).unwrap();
    std::fs::create_dir_all(base.join("docs/human")).unwrap();
    std::fs::write(base.join("Cargo.toml"), b"[package]").unwrap();
    std::fs::write(base.join("src/lib.rs"), b"// lib").unwrap();
    std::fs::write(base.join("src/core.rs"), b"// core").unwrap();
    std::fs::write(base.join("tests/it.rs"), b"// test").unwrap();
    std::fs::write(base.join("docs/human/overview.md"), b"# ov").unwrap();
    let paths = collect_plugin_context_paths(root, "root", &["root".to_string()])
        .expect("context paths gathered");
    assert!(paths
        .all_paths
        .contains(&"plugins/root/Cargo.toml".to_string()));
    assert!(paths
        .all_paths
        .contains(&"plugins/root/src/lib.rs".to_string()));
    // Focus paths are a filtered subset of all_paths (root manifest + src).
    assert!(paths
        .focus_paths
        .contains(&"plugins/root/Cargo.toml".to_string()));
    assert!(paths
        .focus_paths
        .iter()
        .all(|p| paths.all_paths.contains(p)));
}

// ── collect_focus_context_paths: depth 1-2 focus-plugin loop
//    (via collect_plugin_context_paths) ──────────────────────────────
#[test]
fn collect_plugin_context_paths_includes_depth_one_focus_child() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    // Root plugin.
    let root_dir = root.join("plugins/root");
    std::fs::create_dir_all(root_dir.join("src")).unwrap();
    std::fs::write(root_dir.join("Cargo.toml"), b"[package]").unwrap();
    std::fs::write(root_dir.join("src/lib.rs"), b"// root lib").unwrap();
    // Depth-1 child plugin (root/child) — exercises the focus_plugins loop
    // body in collect_focus_context_paths (depth in 1..=2).
    let child_dir = root.join("plugins/root/child");
    std::fs::create_dir_all(child_dir.join("src")).unwrap();
    std::fs::write(child_dir.join("Cargo.toml"), b"[package]").unwrap();
    std::fs::write(child_dir.join("src/core.rs"), b"// child core").unwrap();

    let paths = collect_plugin_context_paths(
        root,
        "root",
        &["root".to_string(), "root/child".to_string()],
    )
    .expect("context paths gathered");
    // The depth-1 child's source entries land in the focus set.
    assert!(paths
        .focus_paths
        .contains(&"plugins/root/child/Cargo.toml".to_string()));
    assert!(paths
        .focus_paths
        .contains(&"plugins/root/child/src/core.rs".to_string()));
}

// ── cleanup_stale_snapshot_dirs (6669 / current 6878-6905) ──────────
#[test]
fn cleanup_stale_snapshot_dirs_removes_dead_and_keeps_live() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    // Missing root → early return (read_dir Err), no panic.
    cleanup_stale_snapshot_dirs(&root.join("does-not-exist"));

    // A directory owned by a dead pid (u32::MAX never a live pid) → removed.
    let dead = root.join(format!("snapshot-{}-123", u32::MAX));
    std::fs::create_dir_all(&dead).unwrap();
    // Old nanos-only format (first segment not a u32 pid) → treated stale.
    let old_format = root.join("snapshot-9999999999999999999-abc");
    std::fs::create_dir_all(&old_format).unwrap();
    // A live-owner dir named with THIS process's pid → kept.
    let live = root.join(make_snapshot_dir_name());
    std::fs::create_dir_all(&live).unwrap();
    // A non-snapshot dir and a plain file → skipped, left intact.
    let other_dir = root.join("not-a-snapshot");
    std::fs::create_dir_all(&other_dir).unwrap();
    let stray_file = root.join("snapshot-1-file");
    std::fs::write(&stray_file, b"x").unwrap();

    cleanup_stale_snapshot_dirs(root);

    assert!(!dead.exists(), "dead-owner snapshot dir must be removed");
    assert!(!old_format.exists(), "legacy-format dir must be removed");
    assert!(live.exists(), "live-owner snapshot dir must be kept");
    assert!(other_dir.exists(), "non-snapshot dir must be untouched");
    assert!(
        stray_file.exists(),
        "non-dir snapshot entry must be untouched"
    );
}

// ── workspace_manifest_lock::acquire (6726-6766 / current 6934-6968) ─
#[test]
fn workspace_manifest_lock_acquire_creates_parent_and_locks() {
    let temp = tempfile::tempdir().expect("tempdir");
    // Parent directory does not exist yet → acquire create_dir_all's it,
    // then opens+flocks the lock file. Guard drop releases the lock.
    let lock_path = temp.path().join("nested/dir/.workspace-manifest.lock");
    {
        let _guard = workspace_manifest_lock::acquire(&lock_path);
        assert!(lock_path.exists(), "lock file must be created");
        assert!(lock_path.parent().unwrap().exists());
    }
    // Re-acquire after the first guard dropped (lock released) succeeds.
    let _guard2 = workspace_manifest_lock::acquire(&lock_path);
    assert!(lock_path.exists());
}

// ── build_snapshot_with_staged_root: PluginUnavailable branch
//    (6161-6164 / current 6370-6378) ─────────────────────────────────
#[test]
fn build_snapshot_errors_when_a_plugin_is_unavailable() {
    use crate::core::models::{
        AbiFingerprint, ArtifactIndex, ArtifactIndexEntry, ArtifactKind, LoaderBudget, PluginDocs,
        ARTIFACT_INDEX_SCHEMA_VERSION,
    };
    use crate::plugin::loader::{Loader, LoaderConfig};

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let index_path = root.join("artifacts/index.json");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();

    // Single plugin whose artifact file does not exist on disk → the loader
    // marks it Unavailable(ArtifactMissing) rather than aborting, and
    // build_snapshot_with_staged_root then converts that into a
    // RuntimeError::PluginUnavailable.
    let docs = PluginDocs {
        plugin_id: "lonely".to_string(),
        plugin_path: "lonely".to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: None,
        nodes: Vec::new(),
        system_hint: None,
    };
    let entry = ArtifactIndexEntry {
        plugin_path: "lonely".to_string(),
        version: "0.1.0".to_string(),
        abi_fingerprint: AbiFingerprint::current_build("crate_lonely_v1", "api_v2"),
        // Points at a file that will never exist under the temp root.
        artifact_path: "lonely.json".to_string(),
        sha256: "0".repeat(64),
        built_at: "0".to_string(),
        parent: None,
        required: false,
        grants_from_parent: Vec::new(),
        docs,
        exports: Vec::new(),
        execution: None,
        artifact_kind: ArtifactKind::Json,
        build_fingerprint: "bf".to_string(),
        input_probe: Default::default(),
        local_path_deps: Vec::new(),
    };
    let index = ArtifactIndex {
        schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
        generated_at: "now".to_string(),
        topo_order: vec!["lonely".to_string()],
        entries: vec![entry],
    };
    std::fs::write(&index_path, serde_json::to_string(&index).unwrap()).unwrap();

    let loader = Loader::new(LoaderConfig {
        plugins_root: root.join("plugins"),
        artifact_index_path: index_path,
        budget: LoaderBudget {
            max_total_plugins: 16,
            max_total_nodes: 64,
            load_timeout_ms: 30_000,
        },
    });
    let staged = root.join("staged");
    let err = build_snapshot_with_staged_root(&loader, staged.clone(), &BTreeMap::new())
        .expect_err("unavailable plugin must surface PluginUnavailable");
    assert!(
        matches!(&err, RuntimeError::PluginUnavailable { plugin_path, required, .. }
                if plugin_path == "lonely" && !*required),
        "unexpected: {err:?}"
    );
    // The staged artifact root is cleaned up on the error path.
    assert!(!staged.exists(), "staged root must be removed on failure");
}

// ── render_child_plugin_core: empty PascalCase part (5874) ──────────
#[test]
fn render_child_plugin_core_handles_empty_dash_segment() {
    // A trailing/leading/double dash produces an empty split part, driving
    // the `None => String::new()` arm of the chars().next() match.
    let core = render_child_plugin_core("foo--bar");
    // Empty middle part contributes nothing; result is still "FooBar".
    assert!(core.contains("pub enum FooBarError"));
    assert!(core.contains("pub struct FooBarPlugin"));
}

// ── plugin_change_reasons: grants / load_result / fingerprint_diff
//    arms (6104, 6107, 6113) ─────────────────────────────────────────
#[test]
fn plugin_change_reasons_reports_grants_load_result_and_fingerprint() {
    use crate::core::models::PluginLoadResult;
    let base = crate::plugin::registry::RegisteredPlugin {
        plugin_path: "root/child".to_string(),
        parent: None,
        required: false,
        grants_from_parent: BTreeSet::new(),
        load_result: PluginLoadResult::Loaded,
        docs: None,
        artifact_path: None,
        artifact_kind: None,
        abi_fingerprint: None,
        execution: None,
        fingerprint_diff: Vec::new(),
    };

    let mut grants = base.clone();
    grants.grants_from_parent = BTreeSet::from(["fs".to_string()]);
    assert_eq!(
        plugin_change_reasons(&base, &grants),
        vec!["grants_changed".to_string()]
    );

    let mut load = base.clone();
    load.load_result =
        PluginLoadResult::Unavailable(crate::core::models::PluginUnavailableReason::InitFailed);
    assert_eq!(
        plugin_change_reasons(&base, &load),
        vec!["load_result_changed".to_string()]
    );

    let mut fp = base.clone();
    fp.fingerprint_diff = vec!["crate_hash".to_string()];
    assert_eq!(
        plugin_change_reasons(&base, &fp),
        vec!["fingerprint_diff_changed".to_string()]
    );
}

// ── atomic_write_bytes: no-filename error + rename success (7019,
//    plus the create_dir_all/write happy path) ──────────────────────
#[test]
fn atomic_write_bytes_errors_when_target_has_no_filename() {
    // Root path "/" has no file_name → InvalidInput error (7019-7022).
    let err =
        atomic_write_bytes(std::path::Path::new("/"), b"x").expect_err("root path has no filename");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn atomic_write_bytes_writes_and_replaces_atomically() {
    let temp = tempfile::tempdir().expect("tempdir");
    // Nested parent is created on demand; file is written then a second
    // write replaces it via the rename path.
    let target = temp.path().join("nested/dir/out.json");
    atomic_write_bytes(&target, b"first").expect("first write");
    assert_eq!(std::fs::read(&target).unwrap(), b"first");
    atomic_write_bytes(&target, b"second").expect("replace write");
    assert_eq!(std::fs::read(&target).unwrap(), b"second");
}

#[test]
fn atomic_write_bytes_cleans_tmp_when_rename_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    // Target is a NON-EMPTY directory: rename(tmp_file -> dir) fails with
    // ENOTEMPTY/EISDIR, driving the cleanup + error-return arm (7030-7032).
    let target = temp.path().join("target-dir");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("occupant"), b"x").unwrap();

    let err =
        atomic_write_bytes(&target, b"payload").expect_err("rename onto a non-empty dir must fail");
    // The error is surfaced (kind varies by platform).
    let _ = err;
    // The temp sidecar file was removed on the failure path.
    let tmp = temp
        .path()
        .join(format!("target-dir.cordis-tmp.{}", std::process::id()));
    assert!(
        !tmp.exists(),
        "temp sidecar must be cleaned up on rename failure"
    );
}

// ── workspace_manifest_lock::acquire: open-failure fallback
//    (6946-6951) ─────────────────────────────────────────────────────
#[test]
fn workspace_manifest_lock_acquire_open_failure_returns_fileless_guard() {
    let temp = tempfile::tempdir().expect("tempdir");
    // Point the lock path at an existing *directory*: OpenOptions::open
    // fails (cannot open a dir read/write), so acquire logs and returns a
    // Guard with `file: None`. create_dir_all(parent) succeeds first.
    let dir_as_lock = temp.path().join("iam-a-directory");
    std::fs::create_dir_all(&dir_as_lock).unwrap();
    // Must not panic; Drop of the fileless guard is a no-op.
    let _guard = workspace_manifest_lock::acquire(&dir_as_lock);
}

// ── normalize_warning_source_path: RootDir component arm (5998) ─────
#[test]
fn normalize_warning_source_path_rejects_retained_root_component() {
    // An empty fixtures_root strips no prefix from an absolute path, so the
    // leading RootDir component survives into the loop and hits the
    // `RootDir | Prefix(_) => return None` arm.
    assert_eq!(
        normalize_warning_source_path("/abs/foo.rs", std::path::Path::new("")),
        None
    );
}
