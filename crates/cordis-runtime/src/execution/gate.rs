//! Gate policy evaluation.
//! Scheduler can call this module to determine whether to wait, complete, or cancel branches.

use crate::core::models::{GatePolicy, NodeOutcome};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BackoffPolicy {
    #[default]
    None,
    Fixed { delay_ms: u64 },
    Exponential { base_ms: u64, max_ms: u64 },
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
        && upstream_nodes.iter().all(|n| {
            outcomes
                .get(n)
                .is_some_and(|o| is_terminal(*o))
        });
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
            if !is_terminal(*outcome) {
                continue;
            }
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

fn is_terminal(outcome: NodeOutcome) -> bool {
    matches!(
        outcome,
        NodeOutcome::Success
            | NodeOutcome::Failure
            | NodeOutcome::Timeout
            | NodeOutcome::Cancelled
            | NodeOutcome::Skipped
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::GatePolicy;

    fn outcomes(pairs: &[(&str, NodeOutcome)]) -> BTreeMap<String, NodeOutcome> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect()
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
        match evaluate_gate(GatePolicy::FirstSuccess, &up, &out, &order) {
            GateDecision::CompleteAndCancel { success, cancel_nodes } => {
                assert!(success);
                assert_eq!(cancel_nodes, vec!["c".to_string()]);
            }
            d => panic!("expected CompleteAndCancel, got {d:?}"),
        }
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
}
