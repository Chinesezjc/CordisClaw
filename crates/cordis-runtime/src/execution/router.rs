//! Router executor for subgraph boundary semantics.
//! It applies `begin_subgraph -> run -> commit/rollback` with dedicated metrics.

use crate::context::{ContextTxn, RuntimeContext};
use crate::core::error::RuntimeError;
use crate::core::models::NodeOutcome;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouterMetrics {
    pub router_execute_total: u64,
    pub router_success_total: u64,
    pub router_failure_total: u64,
    pub router_timeout_total: u64,
    pub router_cancelled_total: u64,
    pub router_skipped_total: u64,
    pub router_overlay_commit_total: u64,
    pub router_overlay_rollback_total: u64,
    pub router_exec_ms: u128,
}

#[derive(Debug, Clone)]
pub struct RouterRunResult {
    pub outcome: NodeOutcome,
}

pub fn execute_router<F>(
    context: &mut RuntimeContext,
    subgraph_id: &str,
    metrics: &mut RouterMetrics,
    run: F,
    timeout_ms: u64,
) -> Result<RouterRunResult, RuntimeError>
where
    F: FnOnce(&RuntimeContext) -> NodeOutcome,
{
    metrics.router_execute_total += 1;
    let started_at = Instant::now();

    context.begin_subgraph(subgraph_id)?;
    let raw_outcome = run(&*context);
    let elapsed = started_at.elapsed();
    // P1-7: only downgrade to Timeout when the run itself did NOT succeed.
    // The previous behaviour overrode Success with Timeout whenever the
    // elapsed time exceeded `timeout_ms` — a slow but correct run got
    // rolled back the same as a genuine timeout. Now Success is preserved
    // and only Failure/Cancelled/Skipped are re-classified as Timeout when
    // the deadline was blown; the caller can still see slow-successes via
    // metrics if it cares.
    let outcome = if timeout_ms > 0
        && elapsed > std::time::Duration::from_millis(timeout_ms)
        && raw_outcome != NodeOutcome::Success
    {
        NodeOutcome::Timeout
    } else {
        raw_outcome
    };

    match outcome {
        NodeOutcome::Success => {
            context.commit_overlay(subgraph_id)?;
            metrics.router_success_total += 1;
            metrics.router_overlay_commit_total += 1;
        }
        NodeOutcome::Failure => {
            context.rollback_overlay(subgraph_id)?;
            metrics.router_failure_total += 1;
            metrics.router_overlay_rollback_total += 1;
        }
        NodeOutcome::Timeout => {
            context.rollback_overlay(subgraph_id)?;
            metrics.router_timeout_total += 1;
            metrics.router_overlay_rollback_total += 1;
        }
        NodeOutcome::Cancelled => {
            context.rollback_overlay(subgraph_id)?;
            metrics.router_cancelled_total += 1;
            metrics.router_overlay_rollback_total += 1;
        }
        NodeOutcome::Skipped => {
            context.rollback_overlay(subgraph_id)?;
            metrics.router_skipped_total += 1;
            metrics.router_overlay_rollback_total += 1;
        }
    }

    metrics.router_exec_ms += elapsed.as_millis();

    Ok(RouterRunResult { outcome })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::RuntimeContext;

    #[test]
    fn slow_success_is_not_downgraded_to_timeout() {
        // P1-7 regression guard: a run that returns Success but happens to
        // exceed `timeout_ms` used to be re-classified as Timeout and
        // rolled back. Now Success is preserved.
        let mut ctx = RuntimeContext::default();
        let mut metrics = RouterMetrics::default();
        let result = execute_router(
            &mut ctx,
            "slow-success",
            &mut metrics,
            |_ctx| {
                std::thread::sleep(std::time::Duration::from_millis(30));
                NodeOutcome::Success
            },
            10,
        )
        .expect("router should not error");
        assert_eq!(result.outcome, NodeOutcome::Success);
        assert_eq!(metrics.router_success_total, 1);
        assert_eq!(metrics.router_timeout_total, 0);
    }

    #[test]
    fn slow_failure_is_reclassified_as_timeout() {
        // The Timeout override still applies to non-Success outcomes.
        let mut ctx = RuntimeContext::default();
        let mut metrics = RouterMetrics::default();
        let result = execute_router(
            &mut ctx,
            "slow-failure",
            &mut metrics,
            |_ctx| {
                std::thread::sleep(std::time::Duration::from_millis(30));
                NodeOutcome::Failure
            },
            10,
        )
        .expect("router should not error");
        assert_eq!(result.outcome, NodeOutcome::Timeout);
        assert_eq!(metrics.router_timeout_total, 1);
    }

    /// Drive every `match outcome` arm through a SINGLE closure type (one
    /// monomorphization of `execute_router`) so that this instantiation
    /// executes all of Success/Failure/Timeout/Cancelled/Skipped. The
    /// closure returns a scripted outcome per call; timeout is disabled so
    /// each requested outcome reaches its own arm verbatim, except the
    /// dedicated slow call that provokes the Timeout reclassification.
    #[test]
    fn every_outcome_arm_is_covered_in_one_instantiation() {
        use std::cell::Cell;
        let script = [
            NodeOutcome::Success,
            NodeOutcome::Failure,
            NodeOutcome::Cancelled,
            NodeOutcome::Skipped,
        ];
        let idx = Cell::new(0usize);
        let run = |_ctx: &RuntimeContext| {
            let i = idx.get();
            idx.set(i + 1);
            script[i]
        };

        let mut metrics = RouterMetrics::default();
        for (i, expected) in script.iter().enumerate() {
            let mut ctx = RuntimeContext::default();
            let result = execute_router(&mut ctx, &format!("sg{i}"), &mut metrics, run, 0)
                .expect("router should not error");
            assert_eq!(&result.outcome, expected);
        }
        assert_eq!(metrics.router_success_total, 1);
        assert_eq!(metrics.router_failure_total, 1);
        assert_eq!(metrics.router_cancelled_total, 1);
        assert_eq!(metrics.router_skipped_total, 1);
        assert_eq!(metrics.router_overlay_commit_total, 1);
        assert_eq!(metrics.router_overlay_rollback_total, 3);

        // A separate slow call (same closure type would extend the script; use
        // an independent closure that still shares no arm gaps) drives the
        // Timeout arm: a non-Success outcome past the deadline is
        // re-classified and rolled back.
        let mut ctx = RuntimeContext::default();
        let slow = |_ctx: &RuntimeContext| {
            std::thread::sleep(std::time::Duration::from_millis(30));
            NodeOutcome::Failure
        };
        let result = execute_router(&mut ctx, "sg-timeout", &mut metrics, slow, 10)
            .expect("router should not error");
        assert_eq!(result.outcome, NodeOutcome::Timeout);
        assert_eq!(metrics.router_timeout_total, 1);
    }

    /// Drive each non-timeout terminal outcome through `execute_router` with a
    /// generous (non-blown) deadline so the raw outcome is preserved, and
    /// assert the matching rollback/commit metric arm fires. Covers the
    /// Failure / Cancelled / Skipped arms that the timeout tests don't reach.
    fn run_fast(outcome: NodeOutcome) -> RouterMetrics {
        let mut ctx = RuntimeContext::default();
        let mut metrics = RouterMetrics::default();
        let result = execute_router(&mut ctx, "sg", &mut metrics, move |_ctx| outcome, 10_000)
            .expect("router should not error");
        assert_eq!(result.outcome, outcome);
        metrics
    }

    #[test]
    fn fast_failure_rolls_back_and_counts() {
        let m = run_fast(NodeOutcome::Failure);
        assert_eq!(m.router_failure_total, 1);
        assert_eq!(m.router_overlay_rollback_total, 1);
        assert_eq!(m.router_success_total, 0);
    }

    #[test]
    fn fast_cancelled_rolls_back_and_counts() {
        let m = run_fast(NodeOutcome::Cancelled);
        assert_eq!(m.router_cancelled_total, 1);
        assert_eq!(m.router_overlay_rollback_total, 1);
    }

    #[test]
    fn fast_skipped_rolls_back_and_counts() {
        let m = run_fast(NodeOutcome::Skipped);
        assert_eq!(m.router_skipped_total, 1);
        assert_eq!(m.router_overlay_rollback_total, 1);
    }
}
