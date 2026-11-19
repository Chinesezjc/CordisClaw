use super::{
    edges_to_net_specs, extract_response_field, infer_outcome_from_payload,
    is_warning_block_boundary, normalize_warning_source_path, parse_response_payload,
    plugin_change_reasons, plugin_iteration_status_from_history, plugin_path_from_runtime_error,
    plugin_relative_depth, select_registered_net_subgraph, stop_result_diagnostic,
    strip_rust_span_suffix, truncate_warning_block, warning_cleanup_error_message,
    warning_path_aliases,
};
use crate::core::error::RuntimeError;
use crate::core::models::NodeOutcome;
use crate::execution::net::ArcDirection;
use crate::kernel::plugin_iteration::{
    CanaryVerdict, PluginIterationFinalVerdict, PluginIterationHistoryEntry, VerifierVerdict,
};
use crate::plugin::abi::PluginResponse;
use crate::plugin::registry::RegisteredPlugin;
use crate::service::graph_registry::{RegisteredNet, RegisteredNetEdge, RegisteredNetEdgeKind};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn parse_response_payload_parses_json_or_falls_back_to_string() {
    assert_eq!(parse_response_payload("{\"a\":1}"), json!({"a": 1}));
    // Non-JSON input becomes a JSON string verbatim.
    assert_eq!(parse_response_payload("not json"), json!("not json"));
}

#[test]
fn stop_result_diagnostic_ok_true_is_silent() {
    // A stop that reports ok=true produces no diagnostic.
    let resp = Ok(PluginResponse {
        payload: r#"{"ok": true}"#.to_string(),
    });
    assert_eq!(stop_result_diagnostic("svc::svc_serve", &resp), None);
}

#[test]
fn stop_result_diagnostic_ok_false_reports_message() {
    // ok=false with a message surfaces the patch-format diagnostic.
    let resp = Ok(PluginResponse {
        payload: r#"{"ok": false, "message": "threads still alive"}"#.to_string(),
    });
    assert_eq!(
        stop_result_diagnostic("svc::svc_serve", &resp),
        Some(
            "[reload] Task node svc::svc_serve stop returned ok=false: threads still alive"
                .to_string()
        )
    );
}

#[test]
fn stop_result_diagnostic_ok_false_without_message_uses_placeholder() {
    // ok=false with no message field falls back to "(no message)".
    let resp = Ok(PluginResponse {
        payload: r#"{"ok": false}"#.to_string(),
    });
    assert_eq!(
        stop_result_diagnostic("svc::svc_serve", &resp),
        Some("[reload] Task node svc::svc_serve stop returned ok=false: (no message)".to_string())
    );
}

#[test]
fn stop_result_diagnostic_missing_ok_or_unparseable_reports() {
    // A payload with no `ok` field is treated as failure.
    let missing_ok = Ok(PluginResponse {
        payload: r#"{"status": "done"}"#.to_string(),
    });
    assert_eq!(
        stop_result_diagnostic("svc::svc_serve", &missing_ok),
        Some("[reload] Task node svc::svc_serve stop returned ok=false: (no message)".to_string())
    );
    // Unparseable payload -> Value::default() (Null) -> failure.
    let garbage = Ok(PluginResponse {
        payload: "not json at all".to_string(),
    });
    assert_eq!(
        stop_result_diagnostic("svc::svc_serve", &garbage),
        Some("[reload] Task node svc::svc_serve stop returned ok=false: (no message)".to_string())
    );
}

#[test]
fn stop_result_diagnostic_invoke_error_reports_error() {
    // An invoke Err surfaces the invoke-failed diagnostic carrying the
    // error text.
    let err: Result<PluginResponse, RuntimeError> = Err(RuntimeError::InvalidArgument {
        message: "boom".to_string(),
    });
    let diag = stop_result_diagnostic("svc::svc_serve", &err)
        .expect("invoke error must produce a diagnostic");
    assert!(
        diag.starts_with("[reload] Task node svc::svc_serve stop invoke failed: "),
        "unexpected diagnostic: {diag}"
    );
    assert!(
        diag.contains("boom"),
        "diagnostic must carry the error text, got: {diag}"
    );
}

#[test]
fn extract_response_field_reads_object_field_only() {
    let payload = json!({"result": {"ok": true}, "n": 3});
    assert_eq!(extract_response_field(&payload, "n"), Some(json!(3)));
    assert_eq!(extract_response_field(&payload, "missing"), None);
    // Non-object payloads yield None.
    assert_eq!(extract_response_field(&json!([1, 2]), "n"), None);
}

#[test]
fn infer_outcome_from_payload_covers_success_and_failure_arms() {
    // Non-object -> Success.
    assert_eq!(
        infer_outcome_from_payload(&json!("x")),
        NodeOutcome::Success
    );
    // ok:false -> Failure.
    assert_eq!(
        infer_outcome_from_payload(&json!({"ok": false})),
        NodeOutcome::Failure
    );
    // Non-empty error string -> Failure.
    assert_eq!(
        infer_outcome_from_payload(&json!({"error": "boom"})),
        NodeOutcome::Failure
    );
    // Blank error string is ignored -> Success.
    assert_eq!(
        infer_outcome_from_payload(&json!({"error": "   "})),
        NodeOutcome::Success
    );
    // Plain object with no failure markers -> Success.
    assert_eq!(
        infer_outcome_from_payload(&json!({"value": 1})),
        NodeOutcome::Success
    );
}

#[test]
fn strip_rust_span_suffix_strips_line_col_when_present() {
    assert_eq!(strip_rust_span_suffix("src/lib.rs:12:5"), "src/lib.rs");
    // Trailing whitespace is trimmed.
    assert_eq!(strip_rust_span_suffix("  src/core.rs:1:1  "), "src/core.rs");
    // No numeric span -> returned trimmed as-is.
    assert_eq!(strip_rust_span_suffix("src/lib.rs"), "src/lib.rs");
    assert_eq!(
        strip_rust_span_suffix("src/lib.rs:foo:bar"),
        "src/lib.rs:foo:bar"
    );
}

#[test]
fn warning_path_aliases_adds_plugins_stripped_alias() {
    assert_eq!(
        warning_path_aliases("plugins/foo/src/lib.rs"),
        vec![
            "plugins/foo/src/lib.rs".to_string(),
            "foo/src/lib.rs".to_string()
        ]
    );
    // Paths without the prefix produce a single alias.
    assert_eq!(
        warning_path_aliases("foo/src/lib.rs"),
        vec!["foo/src/lib.rs".to_string()]
    );
}

#[test]
fn is_warning_block_boundary_recognizes_cargo_markers() {
    for marker in [
        "error: bad",
        "Compiling foo",
        "Checking foo",
        "Finished dev",
        "Running tests",
        "running 3 tests",
        "test result: ok",
        "Doc-tests foo",
    ] {
        assert!(
            is_warning_block_boundary(marker),
            "expected boundary: {marker}"
        );
    }
    assert!(!is_warning_block_boundary("note: something"));
    assert!(!is_warning_block_boundary("  = help: x"));
}

#[test]
fn truncate_warning_block_appends_ellipsis_only_when_truncated() {
    assert_eq!(truncate_warning_block("short", 10), "short");
    assert_eq!(truncate_warning_block("abcdef", 3), "abc...");
}

#[test]
fn warning_cleanup_error_message_embeds_command_and_capped_excerpt() {
    let warnings = vec![
        "warning: one".to_string(),
        "warning: two".to_string(),
        "warning: three".to_string(),
    ];
    let message = warning_cleanup_error_message("cargo build", &warnings);
    assert!(message.contains("verification command `cargo build` succeeded"));
    assert!(message.contains("warning: one"));
    assert!(message.contains("warning: two"));
    // Only the first two warnings are included in the excerpt.
    assert!(!message.contains("warning: three"));
}

#[test]
fn plugin_relative_depth_measures_subtree_distance() {
    assert_eq!(plugin_relative_depth("a/b", "a/b"), Some(0));
    assert_eq!(plugin_relative_depth("a/b", "a/b/c"), Some(1));
    assert_eq!(plugin_relative_depth("a/b", "a/b/c/d"), Some(2));
    // Not under the root.
    assert_eq!(plugin_relative_depth("a/b", "x/y"), None);
    // Prefix collision that is not a real path child.
    assert_eq!(plugin_relative_depth("a/b", "a/bc"), None);
}

#[test]
fn select_registered_net_subgraph_walks_upstream_edges() {
    let edge = |from: &str, to: &str| RegisteredNetEdge {
        from: from.to_string(),
        to: to.to_string(),
        kind: RegisteredNetEdgeKind::Data,
        label: None,
    };
    let net = RegisteredNet {
        nodes: Vec::new(),
        edges: vec![edge("a", "b"), edge("b", "c"), edge("x", "y")],
        diagnostics: Vec::new(),
    };
    // Target c pulls in its transitive upstream (b, a) but not the
    // disconnected x/y component.
    let selected = select_registered_net_subgraph(&net, "c");
    let mut got = selected.into_iter().collect::<Vec<_>>();
    got.sort();
    assert_eq!(got, vec!["a", "b", "c"]);
}

#[test]
fn edges_to_net_specs_builds_place_and_arc_pair_per_selected_edge() {
    // Data edge with a label, plus a control edge without a label, plus an
    // edge whose endpoints are not both selected (must be skipped).
    let edges = vec![
        RegisteredNetEdge {
            from: "a".to_string(),
            to: "b".to_string(),
            kind: RegisteredNetEdgeKind::Data,
            label: Some("payload".to_string()),
        },
        RegisteredNetEdge {
            from: "b".to_string(),
            to: "c".to_string(),
            kind: RegisteredNetEdgeKind::Control,
            label: None,
        },
        // Endpoint `z` is not selected → edge contributes nothing.
        RegisteredNetEdge {
            from: "b".to_string(),
            to: "z".to_string(),
            kind: RegisteredNetEdgeKind::Data,
            label: Some("dropped".to_string()),
        },
    ];
    let selected = BTreeSet::from(["a".to_string(), "b".to_string(), "c".to_string()]);
    let (places, arcs) = edges_to_net_specs(&edges, &selected);

    // Two surviving edges → two places, four arcs.
    let place_ids = places
        .iter()
        .map(|place| place.place_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        place_ids,
        vec![
            "place::a::b::payload".to_string(),
            // Missing label falls back to the literal "control" segment.
            "place::b::c::control".to_string(),
        ]
    );
    assert_eq!(arcs.len(), 4);

    // Labelled data edge a→b.
    let out_ab = &arcs[0];
    assert_eq!(out_ab.arc_id, "arc::a::out::place::a::b::payload");
    assert_eq!(out_ab.place_id, "place::a::b::payload");
    assert_eq!(out_ab.transition_id, "a");
    assert!(matches!(out_ab.direction, ArcDirection::TransitionToPlace));
    assert_eq!(out_ab.label.as_deref(), Some("payload"));
    // Outbound arc is never required.
    assert!(!out_ab.required);

    let in_ab = &arcs[1];
    assert_eq!(in_ab.arc_id, "arc::b::in::place::a::b::payload");
    assert_eq!(in_ab.transition_id, "b");
    assert!(matches!(in_ab.direction, ArcDirection::PlaceToTransition));
    // Data edge → inbound arc is required.
    assert!(in_ab.required);

    // Control edge b→c: inbound arc must NOT be required, label is None.
    let in_bc = &arcs[3];
    assert_eq!(in_bc.arc_id, "arc::c::in::place::b::c::control");
    assert!(matches!(in_bc.direction, ArcDirection::PlaceToTransition));
    assert_eq!(in_bc.label, None);
    assert!(!in_bc.required);
}

#[test]
fn edges_to_net_specs_dedups_repeated_place_ids() {
    // Two identical edges collapse to a single place (BTreeSet), but each
    // still emits its own arc pair.
    let edge = RegisteredNetEdge {
        from: "a".to_string(),
        to: "b".to_string(),
        kind: RegisteredNetEdgeKind::Data,
        label: Some("x".to_string()),
    };
    let edges = vec![edge.clone(), edge];
    let selected = BTreeSet::from(["a".to_string(), "b".to_string()]);
    let (places, arcs) = edges_to_net_specs(&edges, &selected);
    assert_eq!(places.len(), 1, "identical place ids are de-duplicated");
    assert_eq!(arcs.len(), 4, "each edge still emits its own arc pair");
}

#[test]
fn edges_to_net_specs_empty_when_no_edges_selected() {
    let edges = vec![RegisteredNetEdge {
        from: "a".to_string(),
        to: "b".to_string(),
        kind: RegisteredNetEdgeKind::Data,
        label: None,
    }];
    // Neither endpoint selected.
    let selected = BTreeSet::from(["x".to_string()]);
    let (places, arcs) = edges_to_net_specs(&edges, &selected);
    assert!(places.is_empty());
    assert!(arcs.is_empty());
}

#[test]
fn normalize_warning_source_path_handles_relative_absolute_and_traversal() {
    let root = Path::new("/workspace/fixtures");
    // Relative path with a rust span suffix: span stripped, kept relative.
    assert_eq!(
        normalize_warning_source_path("plugins/foo/src/lib.rs:12:5", root),
        Some("plugins/foo/src/lib.rs".to_string())
    );
    // Absolute path under the root is made relative.
    assert_eq!(
        normalize_warning_source_path("/workspace/fixtures/plugins/foo/src/lib.rs", root),
        Some("plugins/foo/src/lib.rs".to_string())
    );
    // Absolute path NOT under the root → strip_prefix fails → None.
    assert_eq!(
        normalize_warning_source_path("/elsewhere/foo.rs", root),
        None
    );
    // CurDir components are dropped; ParentDir pops the prior segment.
    assert_eq!(
        normalize_warning_source_path("./a/./b/../c.rs", root),
        Some("a/c.rs".to_string())
    );
    // Leading ParentDir with nothing to pop → None.
    assert_eq!(normalize_warning_source_path("../escape.rs", root), None);
    // Path that normalizes to empty → None.
    assert_eq!(normalize_warning_source_path(".", root), None);
}

#[test]
fn plugin_path_from_runtime_error_extracts_owning_plugin() {
    // Variant carrying `parent`.
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::ChildNotFound {
            parent: "root/parent".to_string(),
            child_source: "missing".to_string(),
        }),
        Some("root/parent".to_string())
    );
    // Variant carrying `plugin_path`.
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::PluginNotRegistered {
            plugin_path: "root/child".to_string(),
        }),
        Some("root/child".to_string())
    );
    // CycleDetected returns the first node of the cycle.
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::CycleDetected {
            cycle: vec!["a".to_string(), "b".to_string()],
        }),
        Some("a".to_string())
    );
    // A variant with no plugin association → None.
    assert_eq!(
        plugin_path_from_runtime_error(&RuntimeError::InvalidArgument {
            message: "no plugin here".to_string(),
        }),
        None
    );
}

#[test]
fn plugin_change_reasons_reports_each_diff_field() {
    use crate::core::models::PluginLoadResult;
    use std::collections::BTreeSet;
    let base = RegisteredPlugin {
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
    // No changes -> empty reasons.
    assert!(plugin_change_reasons(&base, &base).is_empty());

    let mut changed = base.clone();
    changed.required = !base.required;
    assert_eq!(
        plugin_change_reasons(&base, &changed),
        vec!["required_changed".to_string()]
    );

    let mut parent_changed = base.clone();
    parent_changed.parent = Some("root".to_string());
    assert_eq!(
        plugin_change_reasons(&base, &parent_changed),
        vec!["parent_changed".to_string()]
    );
}

#[test]
fn plugin_iteration_status_from_history_copies_fields() {
    let entry = PluginIterationHistoryEntry {
        iteration_id: "iter-1".to_string(),
        issue_id: "issue-1".to_string(),
        root_plugin_path: "root".to_string(),
        target_plugin_paths: vec!["root/child".to_string()],
        source: None,
        summary: "did work".to_string(),
        changed_paths: vec!["root/src/lib.rs".to_string()],
        verifier_verdict: Some(VerifierVerdict::Pass),
        canary_verdict: Some(CanaryVerdict::Partial),
        final_verdict: PluginIterationFinalVerdict::Blocked,
        blocked_reason: Some("canary partial".to_string()),
        observed_at_ms: 10,
        completed_at_ms: 20,
    };
    let status = plugin_iteration_status_from_history(&entry);
    assert_eq!(status.iteration_id, "iter-1");
    assert_eq!(status.issue_id, "issue-1");
    assert_eq!(status.root_plugin_path, "root");
    assert_eq!(status.target_plugin_paths, vec!["root/child".to_string()]);
    assert_eq!(status.summary, "did work");
    assert_eq!(status.changed_paths, vec!["root/src/lib.rs".to_string()]);
    assert_eq!(status.verifier_verdict, Some(VerifierVerdict::Pass));
    assert_eq!(status.canary_verdict, Some(CanaryVerdict::Partial));
    assert_eq!(status.final_verdict, PluginIterationFinalVerdict::Blocked);
    assert_eq!(status.blocked_reason, Some("canary partial".to_string()));
}
