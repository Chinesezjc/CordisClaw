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
}
