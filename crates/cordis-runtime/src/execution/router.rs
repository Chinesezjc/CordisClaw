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
    F: FnOnce(&mut RuntimeContext) -> NodeOutcome,
{
    metrics.router_execute_total += 1;
    let started_at = Instant::now();

    context.begin_subgraph(subgraph_id)?;
    let raw_outcome = run(context);
    let elapsed = started_at.elapsed();
    let outcome = if timeout_ms > 0 && elapsed > std::time::Duration::from_millis(timeout_ms) {
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
