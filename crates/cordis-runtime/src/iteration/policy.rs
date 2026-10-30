//! Pure policy functions for plugin iterations.
//!
//! Nothing here touches the filesystem, the clock or a host: every function is
//! a decision over plain data, which makes the whole policy table unit-testable
//! and keeps `host.rs` free of scattered rules.

use crate::kernel::plugin_iteration::{
    CanaryVerdict, PluginEditOpKind, PluginEditOperation, PluginIterationFinalVerdict,
    VerifierVerdict,
};

/// Tests an iteration touched. Tests are the promotion contract: a code change
/// must ship with one, and editing an *existing* test needs human approval
/// (otherwise a model could "fix" the failing test instead of the code).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TestChanges {
    pub(crate) added: Vec<String>,
    pub(crate) modified: Vec<String>,
}

pub fn is_test_path(path: &str) -> bool {
    path.split('/').any(|segment| segment == "tests")
        || path.ends_with("_test.rs")
        || path.ends_with("tests.rs")
}

pub fn test_changes(operations: &[PluginEditOperation]) -> TestChanges {
    let mut changes = TestChanges::default();
    for operation in operations {
        if !is_test_path(&operation.path) {
            continue;
        }
        match operation.kind {
            PluginEditOpKind::CreateFile => changes.added.push(operation.path.clone()),
            _ => changes.modified.push(operation.path.clone()),
        }
    }
    changes.added.sort();
    changes.modified.sort();
    changes.added.dedup();
    changes.modified.dedup();
    changes
}

/// A docs-only iteration does not need a test: nothing executable changed.
pub fn iteration_changes_docs_only(operations: &[PluginEditOperation]) -> bool {
    !operations.is_empty()
        && operations
            .iter()
            .all(|operation| operation.path.starts_with("docs/") || operation.path.ends_with(".md"))
}

/// Why an iteration cannot record its summary yet (`None` = fine to record).
pub fn summary_rejection(operations: &[PluginEditOperation]) -> Option<&'static str> {
    let changes = test_changes(operations);
    if iteration_changes_docs_only(operations)
        || !changes.added.is_empty()
        || !changes.modified.is_empty()
    {
        return None;
    }
    Some(
        "every code change must come with a test: add or update a file under \
         plugins/<plugin>/tests/ (a docs-only change is exempt). Add the test, run \
         run_plugin_test, then record the summary again.",
    )
}

/// Canary evidence is only meaningful for a *deterministic* invocation: a node
/// that draws randomness (a gacha pull) or reads the clock cannot be compared
/// with a recorded response. Probe the candidate twice and skip the sample when
/// the two runs disagree.
pub fn canary_probe_verdict(
    first: &serde_json::Value,
    second: &serde_json::Value,
    recorded: &serde_json::Value,
) -> Option<CanaryVerdict> {
    (first == second).then(|| {
        if first == recorded {
            CanaryVerdict::Pass
        } else {
            CanaryVerdict::Fail
        }
    })
}

/// Side effects a node declares in its docs. A node that declares any is not
/// replayed as canary evidence: replaying it mutates state (a pull consumes
/// pity) and its response is usually the very thing the edit changes.
/// Does this node's declared effect *mutate* something a replay would repeat?
///
/// Declaring any side effect used to be enough to skip canary replay, which
/// also skipped plain terminal I/O (`reads stdin`, `writes stdout`) — exactly
/// the nodes worth replaying. Only effects that touch persistent state, files
/// or consumables disqualify a sample.
pub fn node_declares_side_effects_in(
    nodes: &[cordis_plugin_sdk::NodeDoc],
    node_id: &str,
) -> bool {
    const MUTATING: [&str; 15] = [
        "consum", "mutat", "persist", "delete", "remove", "increment", "decrement", "save",
        "update", "create", "spend", "charge", "grant", "file", "disk",
    ];
    nodes
        .iter()
        .find(|node| node.id == node_id)
        .map(|node| {
            node.side_effects.iter().any(|effect| {
                let effect = effect.to_ascii_lowercase();
                MUTATING.iter().any(|marker| effect.contains(marker))
            })
        })
        .unwrap_or(false)
}

/// The promotion decision.
///
/// Tests are the contract: promote when the verifier (which runs `cargo test`)
/// passes. An iteration that edits an existing test is staged for approval
/// instead — tests are the contract, so changing them needs a human. The canary
/// is diagnostics only: random/side-effecting nodes made it a coin flip.
pub fn promotion_verdict(
    verifier: VerifierVerdict,
    requires_approval: bool,
    manual_approved: bool,
) -> PluginIterationFinalVerdict {
    match (
        verifier == VerifierVerdict::Pass,
        requires_approval,
        manual_approved,
    ) {
        (false, _, _) => PluginIterationFinalVerdict::RolledBack,
        (true, true, false) => PluginIterationFinalVerdict::Blocked,
        (true, _, _) => PluginIterationFinalVerdict::Promoted,
    }
}

/// Human-readable reason attached to a `Blocked` verdict.
pub(crate) const BLOCKED_EDITS_EXISTING_TEST: &str =
    "this change edits an existing test, and tests are the contract: it needs operator \
     approval. Ask an operator to approve this iteration (approve_iteration).";
#[cfg(test)]
mod test_change_policy_tests {
    use super::*;

    fn op(path: &str, kind: PluginEditOpKind) -> PluginEditOperation {
        PluginEditOperation {
            path: path.to_string(),
            kind,
            expected_old_string: None,
            expected_sha256: None,
            new_content: None,
            pointer: None,
            dotted_key: None,
            value: None,
        }
    }

    #[test]
    fn test_paths_are_recognised() {
        assert!(is_test_path("plugins/gacha/tests/basic.rs"));
        assert!(is_test_path("plugins/gacha/tests/nested/case.rs"));
        assert!(is_test_path("plugins/gacha/src/foo_test.rs"));
        assert!(!is_test_path("plugins/gacha/src/lib.rs"));
        assert!(!is_test_path("plugins/gacha/docs/human/overview.md"));
    }

    // A new test file counts as "added"; touching an existing one counts as
    // "modified" (which needs approval).
    #[test]
    fn created_and_modified_tests_are_separated() {
        let ops = vec![
            op("plugins/gacha/src/lib.rs", PluginEditOpKind::ReplaceExact),
            op(
                "plugins/gacha/tests/new_case.rs",
                PluginEditOpKind::CreateFile,
            ),
            op(
                "plugins/gacha/tests/basic.rs",
                PluginEditOpKind::ReplaceExact,
            ),
            op("plugins/gacha/tests/old.rs", PluginEditOpKind::DeleteFile),
        ];
        let changes = test_changes(&ops);
        assert_eq!(changes.added, vec!["plugins/gacha/tests/new_case.rs"]);
        assert_eq!(
            changes.modified,
            vec!["plugins/gacha/tests/basic.rs", "plugins/gacha/tests/old.rs"]
        );
        // A source-only change has no test evidence at all.
        assert_eq!(
            test_changes(&[op(
                "plugins/gacha/src/lib.rs",
                PluginEditOpKind::ReplaceExact
            )]),
            TestChanges::default()
        );
    }

    #[test]
    fn docs_only_changes_are_exempt_from_the_test_requirement() {
        assert!(iteration_changes_docs_only(&[
            op(
                "plugins/gacha/docs/human/overview.md",
                PluginEditOpKind::ReplaceExact
            ),
            op(
                "docs/architecture/status-and-open-items.md",
                PluginEditOpKind::ReplaceExact
            ),
        ]));
        assert!(!iteration_changes_docs_only(&[
            op(
                "plugins/gacha/docs/human/overview.md",
                PluginEditOpKind::ReplaceExact
            ),
            op("plugins/gacha/src/lib.rs", PluginEditOpKind::ReplaceExact),
        ]));
        assert!(!iteration_changes_docs_only(&[]));
    }
}

#[cfg(test)]
mod canary_probe_tests {
    use super::*;
    use serde_json::json;

    // Random nodes (a gacha pull) must never be canary evidence: the same
    // payload yields different results, which used to fail the canary and roll
    // back every iteration touching such a plugin.
    #[test]
    fn nondeterministic_replay_is_not_evidence() {
        let recorded = json!({"name": "班尼特"});
        let first = json!({"name": "迪奥娜"});
        let second = json!({"name": "卡齐娜"});
        assert_eq!(canary_probe_verdict(&first, &second, &recorded), None);
    }

    #[test]
    fn deterministic_replay_compares_with_the_recorded_response() {
        let recorded = json!({"pity_5": 3});
        assert_eq!(
            canary_probe_verdict(&recorded, &recorded, &recorded),
            Some(CanaryVerdict::Pass)
        );
        // Stable across the probe but different from the baseline → the edit
        // changed behavior, which is what the canary exists to catch.
        let changed = json!({"pity_5": 4});
        assert_eq!(
            canary_probe_verdict(&changed, &changed, &recorded),
            Some(CanaryVerdict::Fail)
        );
    }

    // Nodes that declare side effects are not replayed: replaying a pull would
    // consume pity and its response is exactly what an edit changes.
    #[test]
    fn nodes_with_declared_side_effects_are_skipped() {
        let node = |id: &str, side_effects: Vec<String>| cordis_plugin_sdk::NodeDoc {
            id: id.to_string(),
            summary: id.to_string(),
            input_schema: json!({}),
            output_schema: json!({}),
            side_effects,
            failure_modes: Vec::new(),
            node_type: cordis_plugin_sdk::NodeType::Task,
            agent_accessible: true,
        };
        let nodes = vec![
            node("gacha_entry", vec!["consumes pity".to_string()]),
            node("gacha_status", Vec::new()),
        ];
        // Terminal I/O is not mutation: replaying `echo` twice is harmless,
        // and skipping it left the canary with no evidence at all.
        let io_nodes = vec![
            node("shell_entry", vec!["reads stdin".to_string(), "writes stdout".to_string()]),
            node("writes_files", vec!["writes files".to_string()]),
        ];
        assert!(!node_declares_side_effects_in(&io_nodes, "shell_entry"));
        assert!(node_declares_side_effects_in(&io_nodes, "writes_files"));
        assert!(node_declares_side_effects_in(&nodes, "gacha_entry"));
        assert!(!node_declares_side_effects_in(&nodes, "gacha_status"));
        assert!(!node_declares_side_effects_in(&nodes, "nope"));
    }
}

#[cfg(test)]
mod promotion_verdict_tests {
    use super::{promotion_verdict, PluginIterationFinalVerdict, VerifierVerdict};

    #[test]
    fn tests_decide_and_existing_test_edits_need_approval() {
        use PluginIterationFinalVerdict::{Blocked, Promoted, RolledBack};
        // Tests fail → always rolled back, approved or not.
        assert_eq!(
            promotion_verdict(VerifierVerdict::Fail, false, false),
            RolledBack
        );
        assert_eq!(
            promotion_verdict(VerifierVerdict::Fail, true, true),
            RolledBack
        );
        // Tests pass, no test edits → promote.
        assert_eq!(
            promotion_verdict(VerifierVerdict::Pass, false, false),
            Promoted
        );
        // Tests pass but an existing test was edited → wait for approval.
        assert_eq!(
            promotion_verdict(VerifierVerdict::Pass, true, false),
            Blocked
        );
        // Approval unblocks it.
        assert_eq!(
            promotion_verdict(VerifierVerdict::Pass, true, true),
            Promoted
        );
        // Partial verification is not a pass.
        assert_eq!(
            promotion_verdict(VerifierVerdict::Partial, false, false),
            RolledBack
        );
    }
}
