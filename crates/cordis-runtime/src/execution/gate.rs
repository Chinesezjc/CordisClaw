//! Gate policy evaluation.
//! Scheduler can call this module to determine whether to wait, complete, or cancel branches.

use crate::core::models::{GatePolicy, NodeOutcome};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BackoffPolicy {
    #[default]
    None,
    Fixed {
        delay_ms: u64,
    },
    Exponential {
        base_ms: u64,
        max_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPolicy {
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub backoff: BackoffPolicy,
}

impl Default for RunPolicy {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_retries: 0,
            backoff: BackoffPolicy::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    Wait,
    CompleteSuccess,
    CompleteFailure,
    /// Used by FirstSuccess / FirstCompleted to cancel pending branches.
    CompleteAndCancel {
        success: bool,
        cancel_nodes: Vec<String>,
    },
}

pub fn evaluate_gate(
    policy: GatePolicy,
    upstream_nodes: &[String],
    outcomes: &BTreeMap<String, NodeOutcome>,
    completion_order: &[String],
) -> GateDecision {
    match policy {
        GatePolicy::AllOf => eval_all_of(upstream_nodes, outcomes),
        GatePolicy::AnyOf => eval_any_of(upstream_nodes, outcomes),
        GatePolicy::FirstSuccess => eval_first_success(upstream_nodes, outcomes, completion_order),
        GatePolicy::FirstCompleted => {
            eval_first_completed(upstream_nodes, outcomes, completion_order)
        }
        GatePolicy::AtLeast(k) => eval_at_least(k, upstream_nodes, outcomes),
    }
}

fn eval_all_of(
    upstream_nodes: &[String],
    outcomes: &BTreeMap<String, NodeOutcome>,
) -> GateDecision {
    if upstream_nodes.is_empty() {
        return GateDecision::CompleteSuccess;
    }
    let mut all_success = true;
    for node in upstream_nodes {
        match outcomes.get(node) {
            Some(NodeOutcome::Success) => {}
            Some(NodeOutcome::Failure | NodeOutcome::Timeout) => {
                return GateDecision::CompleteFailure
            }
            Some(NodeOutcome::Cancelled | NodeOutcome::Skipped) | None => all_success = false,
        }
    }
    if all_success {
        GateDecision::CompleteSuccess
    } else {
        GateDecision::Wait
    }
}

fn eval_any_of(
    upstream_nodes: &[String],
    outcomes: &BTreeMap<String, NodeOutcome>,
) -> GateDecision {
    if upstream_nodes.is_empty() {
        return GateDecision::CompleteFailure;
    }
    let mut all_terminal_non_success = true;
    for node in upstream_nodes {
        match outcomes.get(node) {
            Some(NodeOutcome::Success) => return GateDecision::CompleteSuccess,
            Some(
                NodeOutcome::Failure
                | NodeOutcome::Timeout
                | NodeOutcome::Cancelled
                | NodeOutcome::Skipped,
            ) => {}
            None => all_terminal_non_success = false,
        }
    }
    if all_terminal_non_success {
        GateDecision::CompleteFailure
    } else {
        GateDecision::Wait
    }
}

fn eval_first_success(
    upstream_nodes: &[String],
    outcomes: &BTreeMap<String, NodeOutcome>,
    completion_order: &[String],
) -> GateDecision {
    let upstream: BTreeSet<_> = upstream_nodes.iter().cloned().collect();
    let mut first_success: Option<String> = None;

    for node in completion_order {
        if !upstream.contains(node) {
            continue;
        }
        if let Some(outcome) = outcomes.get(node) {
            if *outcome == NodeOutcome::Success {
                first_success = Some(node.clone());
                break;
            }
        }
    }

    if let Some(winner) = first_success {
        let cancel_nodes = upstream_nodes
            .iter()
            .filter(|n| {
                n.as_str() != winner.as_str()
                    && !matches!(outcomes.get(*n), Some(out) if is_terminal(*out))
            })
            .cloned()
            .collect::<Vec<_>>();
        return GateDecision::CompleteAndCancel {
            success: true,
            cancel_nodes,
        };
    }

    // P1-8: count terminal upstreams directly from `outcomes` (source of
    // truth) rather than via `completion_order`. The previous counter only
    // incremented when a node appeared in `completion_order` AND had an
    // outcome recorded, so a terminal-but-not-yet-ordered upstream held the
    // gate in Wait forever. Now: if every upstream has some terminal
    // outcome and none of them are Success, we complete as failure.
    let all_terminal_non_success = !upstream_nodes.is_empty()
        && upstream_nodes
            .iter()
            .all(|n| outcomes.get(n).is_some_and(|o| is_terminal(*o)));
    if all_terminal_non_success {
        GateDecision::CompleteFailure
    } else {
        GateDecision::Wait
    }
}

fn eval_first_completed(
    upstream_nodes: &[String],
    outcomes: &BTreeMap<String, NodeOutcome>,
    completion_order: &[String],
) -> GateDecision {
    let upstream: BTreeSet<_> = upstream_nodes.iter().cloned().collect();
    for node in completion_order {
        if !upstream.contains(node) {
            continue;
        }
        if let Some(outcome) = outcomes.get(node) {
            // Archived math-unreachable guard: every current `NodeOutcome`
            // variant is terminal (see `is_terminal`), so an outcome recorded
            // in `outcomes` is always terminal and this `continue` never runs.
            // Kept as a forward guard for a future non-terminal state
            // (e.g. Running/Pending); it would then skip such upstreams here.
            debug_assert!(
                is_terminal(*outcome),
                "NodeOutcome variants are all terminal"
            );
            let cancel_nodes = upstream_nodes
                .iter()
                .filter(|n| {
                    n.as_str() != node.as_str()
                        && !matches!(outcomes.get(*n), Some(out) if is_terminal(*out))
                })
                .cloned()
                .collect::<Vec<_>>();

            if *outcome == NodeOutcome::Success {
                if cancel_nodes.is_empty() {
                    return GateDecision::CompleteSuccess;
                }
                return GateDecision::CompleteAndCancel {
                    success: true,
                    cancel_nodes,
                };
            }
            if cancel_nodes.is_empty() {
                return GateDecision::CompleteFailure;
            }
            return GateDecision::CompleteAndCancel {
                success: false,
                cancel_nodes,
            };
        }
    }
    GateDecision::Wait
}

fn eval_at_least(
    k: usize,
    upstream_nodes: &[String],
    outcomes: &BTreeMap<String, NodeOutcome>,
) -> GateDecision {
    if k == 0 {
        return GateDecision::CompleteSuccess;
    }
    let mut success = 0usize;
    let mut possible_more = 0usize;
    for node in upstream_nodes {
        match outcomes.get(node) {
            Some(NodeOutcome::Success) => success += 1,
            Some(
                NodeOutcome::Failure
                | NodeOutcome::Timeout
                | NodeOutcome::Cancelled
                | NodeOutcome::Skipped,
            ) => {}
            None => possible_more += 1,
        }
    }
    if success >= k {
        return GateDecision::CompleteSuccess;
    }
    if success + possible_more < k {
        return GateDecision::CompleteFailure;
    }
    GateDecision::Wait
}

/// True when `outcome` is a settled (terminal) node state.
///
/// Every current `NodeOutcome` variant is terminal, so this presently returns
/// `true` for all inputs — the `matches!` false-arm is therefore not coverable
/// today. The exhaustive variant list is kept deliberately (rather than
/// collapsed to `true`) so that adding a non-terminal state (e.g. `Running`)
/// forces a compile-time review of every gate that calls this.
fn is_terminal(outcome: NodeOutcome) -> bool {
    use NodeOutcome::{Cancelled, Failure, Skipped, Success, Timeout};
    matches!(outcome, Success | Failure | Timeout | Cancelled | Skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::GatePolicy;

    fn outcomes(pairs: &[(&str, NodeOutcome)]) -> BTreeMap<String, NodeOutcome> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// P1-8: `eval_first_success` must decide `CompleteFailure` as soon
    /// as every upstream has *some* terminal outcome and none are
    /// Success — even if `completion_order` doesn't happen to list
    /// them. Old code counted via `completion_order` and would Wait
    /// forever if any upstream was terminal-but-not-ordered.
    #[test]
    fn first_success_completes_on_all_terminal_failure_without_order() {
        let up = vec!["a".to_string(), "b".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Failure), ("b", NodeOutcome::Timeout)]);
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            evaluate_gate(GatePolicy::FirstSuccess, &up, &out, &empty),
            GateDecision::CompleteFailure
        );
    }

    #[test]
    fn first_success_returns_wait_when_any_upstream_still_pending() {
        let up = vec!["a".to_string(), "b".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Failure)]);
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            evaluate_gate(GatePolicy::FirstSuccess, &up, &out, &empty),
            GateDecision::Wait
        );
    }

    #[test]
    fn first_success_cancels_pending_when_one_wins() {
        let up = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = outcomes(&[
            ("a", NodeOutcome::Failure),
            ("b", NodeOutcome::Success),
            // c still pending
        ]);
        let order = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            evaluate_gate(GatePolicy::FirstSuccess, &up, &out, &order),
            GateDecision::CompleteAndCancel {
                success: true,
                cancel_nodes: vec!["c".to_string()],
            }
        );
    }

    /// `eval_first_success`: a node listed in `completion_order` that is
    /// upstream but has no recorded outcome yet drives the `None` arm of
    /// `outcomes.get(node)` (the loop skips it), then a later ordered node
    /// carries the Success. This is the only path that reaches the fall-
    /// through past the inner `if let Some(outcome)`.
    #[test]
    fn first_success_skips_ordered_upstream_without_outcome() {
        let up = vec!["a".to_string(), "b".to_string()];
        // `a` is upstream and ordered first but has no outcome; `b` wins.
        let out = outcomes(&[("b", NodeOutcome::Success)]);
        let order = vec!["a".to_string(), "b".to_string()];
        // `a` (upstream, ordered first, no outcome) is skipped in the winner
        // search; `b` wins, and `a` — still non-terminal — is cancelled.
        assert_eq!(
            evaluate_gate(GatePolicy::FirstSuccess, &up, &out, &order),
            GateDecision::CompleteAndCancel {
                success: true,
                cancel_nodes: vec!["a".to_string()],
            }
        );
    }

    #[test]
    fn all_of_success_only_when_every_upstream_success() {
        let up = vec!["a".to_string(), "b".to_string()];
        let all = outcomes(&[("a", NodeOutcome::Success), ("b", NodeOutcome::Success)]);
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            evaluate_gate(GatePolicy::AllOf, &up, &all, &empty),
            GateDecision::CompleteSuccess
        );
        let with_failure = outcomes(&[("a", NodeOutcome::Success), ("b", NodeOutcome::Failure)]);
        assert_eq!(
            evaluate_gate(GatePolicy::AllOf, &up, &with_failure, &empty),
            GateDecision::CompleteFailure
        );
        let partial = outcomes(&[("a", NodeOutcome::Success)]);
        assert_eq!(
            evaluate_gate(GatePolicy::AllOf, &up, &partial, &empty),
            GateDecision::Wait
        );
    }

    #[test]
    fn at_least_k_gates_correctly() {
        let up = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let empty: Vec<String> = Vec::new();
        let two_ok = outcomes(&[
            ("a", NodeOutcome::Success),
            ("b", NodeOutcome::Success),
            ("c", NodeOutcome::Failure),
        ]);
        assert_eq!(
            evaluate_gate(GatePolicy::AtLeast(2), &up, &two_ok, &empty),
            GateDecision::CompleteSuccess
        );
        let one_ok_two_fail = outcomes(&[
            ("a", NodeOutcome::Success),
            ("b", NodeOutcome::Failure),
            ("c", NodeOutcome::Failure),
        ]);
        assert_eq!(
            evaluate_gate(GatePolicy::AtLeast(2), &up, &one_ok_two_fail, &empty),
            GateDecision::CompleteFailure
        );
    }

    #[test]
    fn backoff_policy_default_is_none() {
        assert_eq!(BackoffPolicy::default(), BackoffPolicy::None);
    }

    // `RunPolicy::default` sets the documented defaults: 30s timeout, no
    // retries, no backoff. Exercises the hand-written Default impl.
    #[test]
    fn run_policy_default_values() {
        let p = RunPolicy::default();
        assert_eq!(p.timeout_ms, 30_000);
        assert_eq!(p.max_retries, 0);
        assert_eq!(p.backoff, BackoffPolicy::None);
    }

    // AnyOf: first Success short-circuits to CompleteSuccess; all-terminal
    // non-success collapses to CompleteFailure; a pending upstream forces Wait.
    #[test]
    fn any_of_covers_all_branches() {
        let up = vec!["a".to_string(), "b".to_string()];
        let empty: Vec<String> = Vec::new();
        let one_ok = outcomes(&[("a", NodeOutcome::Success)]);
        assert_eq!(
            evaluate_gate(GatePolicy::AnyOf, &up, &one_ok, &empty),
            GateDecision::CompleteSuccess
        );
        let both_fail = outcomes(&[("a", NodeOutcome::Failure), ("b", NodeOutcome::Cancelled)]);
        assert_eq!(
            evaluate_gate(GatePolicy::AnyOf, &up, &both_fail, &empty),
            GateDecision::CompleteFailure
        );
        let one_fail_one_pending = outcomes(&[("a", NodeOutcome::Failure)]);
        assert_eq!(
            evaluate_gate(GatePolicy::AnyOf, &up, &one_fail_one_pending, &empty),
            GateDecision::Wait
        );
        // Empty upstream is a vacuous failure for AnyOf.
        let none: Vec<String> = Vec::new();
        assert_eq!(
            evaluate_gate(GatePolicy::AnyOf, &none, &empty_map(), &empty),
            GateDecision::CompleteFailure
        );
    }

    fn empty_map() -> BTreeMap<String, NodeOutcome> {
        BTreeMap::new()
    }

    // FirstCompleted: completion_order is still empty even though an outcome
    // is already recorded → nothing to inspect yet → Wait.
    #[test]
    fn first_completed_empty_order_waits() {
        let up = vec!["a".to_string()];
        let empty: Vec<String> = Vec::new();
        let out = outcomes(&[("a", NodeOutcome::Success)]);
        assert_eq!(
            evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &empty),
            GateDecision::Wait
        );
    }

    // FirstCompleted: the first terminal node in completion_order is a failure,
    // and another upstream is still pending → cancel the pending branch and
    // report failure (covers the failure+cancel arm).
    #[test]
    fn first_completed_failure_cancels_pending() {
        let up = vec!["a".to_string(), "b".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Failure)]);
        let order = vec!["a".to_string()];
        assert_eq!(
            evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &order),
            GateDecision::CompleteAndCancel {
                success: false,
                cancel_nodes: vec!["b".to_string()],
            }
        );
    }

    // FirstCompleted: Timeout is a terminal non-success the same way — the
    // pending peer is cancelled and the gate reports failure.
    #[test]
    fn first_completed_timeout_cancels_pending_as_failure() {
        let up = vec!["a".to_string(), "b".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Timeout)]);
        let order = vec!["a".to_string()];
        match evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &order) {
            GateDecision::CompleteAndCancel {
                success,
                cancel_nodes,
            } => {
                assert!(!success);
                assert_eq!(cancel_nodes, vec!["b".to_string()]);
            }
            d => panic!("expected CompleteAndCancel failure, got {d:?}"),
        }
    }

    // FirstCompleted: a lone terminal failure with no pending peers completes
    // as plain failure (empty cancel set).
    #[test]
    fn first_completed_lone_failure_completes_failure() {
        let up = vec!["a".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Failure)]);
        let order = vec!["a".to_string()];
        assert_eq!(
            evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &order),
            GateDecision::CompleteFailure
        );
    }

    // FirstCompleted: lone terminal success with no pending peers → success.
    #[test]
    fn first_completed_lone_success_completes_success() {
        let up = vec!["a".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Success)]);
        let order = vec!["a".to_string()];
        assert_eq!(
            evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &order),
            GateDecision::CompleteSuccess
        );
    }

    // FirstCompleted: success with a pending peer → cancel then success.
    #[test]
    fn first_completed_success_cancels_pending() {
        let up = vec!["a".to_string(), "b".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Success)]);
        let order = vec!["a".to_string()];
        assert_eq!(
            evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &order),
            GateDecision::CompleteAndCancel {
                success: true,
                cancel_nodes: vec!["b".to_string()],
            }
        );
    }

    // FirstCompleted: a foreign completion_order entry (not upstream) and an
    // upstream entry with no recorded outcome are both skipped before the
    // first real terminal decides; the still-pending upstream is cancelled.
    #[test]
    fn first_completed_skips_foreign_and_unrecorded_entries() {
        let up = vec!["a".to_string(), "b".to_string()];
        let out = outcomes(&[("b", NodeOutcome::Success)]);
        let order = vec!["z".to_string(), "a".to_string(), "b".to_string()];
        match evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &order) {
            GateDecision::CompleteAndCancel {
                success,
                cancel_nodes,
            } => {
                assert!(success);
                assert_eq!(cancel_nodes, vec!["a".to_string()]);
            }
            d => panic!("expected CompleteAndCancel, got {d:?}"),
        }
    }

    // FirstCompleted: nothing terminal yet → Wait (fallthrough past the loop).
    #[test]
    fn first_completed_waits_when_nothing_terminal() {
        let up = vec!["a".to_string(), "b".to_string()];
        let out = empty_map();
        let order: Vec<String> = Vec::new();
        assert_eq!(
            evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &order),
            GateDecision::Wait
        );
    }

    // FirstSuccess: a node listed in completion_order but with a non-Success
    // outcome must be skipped (the loop keeps scanning), exercising the
    // fall-through past the success check.
    #[test]
    fn first_success_skips_non_success_in_order() {
        let up = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = outcomes(&[
            ("a", NodeOutcome::Failure),
            ("b", NodeOutcome::Cancelled),
            ("c", NodeOutcome::Success),
        ]);
        let order = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(
            evaluate_gate(GatePolicy::FirstSuccess, &up, &out, &order),
            GateDecision::CompleteAndCancel {
                success: true,
                cancel_nodes: Vec::new(),
            }
        );
    }

    // AllOf over an empty upstream set is vacuously satisfied → success
    // (the `upstream_nodes.is_empty()` early return).
    #[test]
    fn all_of_empty_upstream_is_vacuous_success() {
        let up: Vec<String> = Vec::new();
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            evaluate_gate(GatePolicy::AllOf, &up, &empty_map(), &empty),
            GateDecision::CompleteSuccess
        );
    }

    // FirstSuccess: a completion_order entry that is NOT among the upstream
    // nodes must be skipped by the `!upstream.contains(node)` guard, so a
    // stray ordered id doesn't spuriously win the gate.
    #[test]
    fn first_success_skips_order_entries_outside_upstream() {
        let up = vec!["a".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Success), ("stray", NodeOutcome::Success)]);
        // "stray" precedes "a" in the order but is not an upstream node.
        let order = vec!["stray".to_string(), "a".to_string()];
        assert_eq!(
            evaluate_gate(GatePolicy::FirstSuccess, &up, &out, &order),
            GateDecision::CompleteAndCancel {
                success: true,
                cancel_nodes: Vec::new(),
            }
        );
    }

    // FirstCompleted: same guard — an ordered id outside the upstream set is
    // ignored, and the real upstream terminal decides the gate.
    #[test]
    fn first_completed_skips_order_entries_outside_upstream() {
        let up = vec!["a".to_string()];
        let out = outcomes(&[("a", NodeOutcome::Failure), ("stray", NodeOutcome::Success)]);
        let order = vec!["stray".to_string(), "a".to_string()];
        assert_eq!(
            evaluate_gate(GatePolicy::FirstCompleted, &up, &out, &order),
            GateDecision::CompleteFailure
        );
    }

    // AtLeast(0) is trivially satisfied; a still-reachable quorum waits.
    #[test]
    fn at_least_zero_and_wait_branches() {
        let up = vec!["a".to_string(), "b".to_string()];
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            evaluate_gate(GatePolicy::AtLeast(0), &up, &empty_map(), &empty),
            GateDecision::CompleteSuccess
        );
        // One success, one pending, need two → still reachable → Wait.
        let one = outcomes(&[("a", NodeOutcome::Success)]);
        assert_eq!(
            evaluate_gate(GatePolicy::AtLeast(2), &up, &one, &empty),
            GateDecision::Wait
        );
    }
}
