//! Gate policy evaluation.
//! Scheduler can call this module to determine whether to wait, complete, or cancel branches.

use crate::core::models::{GatePolicy, NodeOutcome};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackoffPolicy {
    None,
    Fixed { delay_ms: u64 },
    Exponential { base_ms: u64, max_ms: u64 },
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self::None
    }
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
    let mut completed_terminals = 0usize;
    let mut first_success: Option<String> = None;

    for node in completion_order {
        if !upstream.contains(node) {
            continue;
        }
        if let Some(outcome) = outcomes.get(node) {
            if is_terminal(*outcome) {
                completed_terminals += 1;
            }
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

    if completed_terminals == upstream_nodes.len() && !upstream_nodes.is_empty() {
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
