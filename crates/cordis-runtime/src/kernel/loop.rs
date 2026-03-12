//! OpenClaw-like self-iteration loop skeleton.
//! It implements: observe -> diagnose -> plan -> apply -> verify -> score -> promote/rollback.

use crate::kernel::evaluator::{EvalHarness, EvaluationReport, VerificationInput};
use crate::kernel::memory::{ChangeMemory, ChangeVerdict};
use crate::kernel::policy::IterationPolicy;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IterationStage {
    Observe,
    Diagnose,
    Plan,
    Apply,
    Verify,
    Score,
    SafetyGate,
    Promote,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IterationInput {
    pub issue_id: String,
    pub patch_id: String,
    pub changed_paths: Vec<String>,
    pub diff_lines: usize,
    pub manual_approved: bool,
    pub verification: VerificationInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IterationReport {
    pub stages: Vec<IterationStage>,
    pub verdict: ChangeVerdict,
    pub evaluation: EvaluationReport,
    pub elapsed_ms: u128,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelLoopMetrics {
    pub iteration_total: u64,
    pub iteration_promote_total: u64,
    pub iteration_rollback_total: u64,
}

#[derive(Debug, Clone)]
pub struct SelfIterationKernel {
    policy: IterationPolicy,
    evaluator: EvalHarness,
    memory: ChangeMemory,
    metrics: KernelLoopMetrics,
}

impl SelfIterationKernel {
    pub fn new(policy: IterationPolicy, evaluator: EvalHarness, memory: ChangeMemory) -> Self {
        Self {
            policy,
            evaluator,
            memory,
            metrics: KernelLoopMetrics::default(),
        }
    }

    pub fn memory(&self) -> &ChangeMemory {
        &self.memory
    }

    pub fn metrics(&self) -> &KernelLoopMetrics {
        &self.metrics
    }

    /// Run one closed-loop iteration with policy checks and scoring.
    pub fn run_once(&mut self, input: IterationInput) -> IterationReport {
        self.metrics.iteration_total += 1;
        let started_at = Instant::now();
        let mut stages = vec![
            IterationStage::Observe,
            IterationStage::Diagnose,
            IterationStage::Plan,
        ];

        let mut eval = self.evaluator.evaluate(input.verification);

        if !self.policy.paths_allowed(&input.changed_paths) {
            eval.accepted = false;
            eval.reasons.push("path_not_allowed".to_string());
        }
        if !self.policy.diff_allowed(input.diff_lines) {
            eval.accepted = false;
            eval.reasons.push(format!(
                "diff_too_large:{}>{}",
                input.diff_lines, self.policy.max_diff_lines
            ));
        }

        stages.push(IterationStage::Apply);
        stages.push(IterationStage::Verify);
        stages.push(IterationStage::Score);
        stages.push(IterationStage::SafetyGate);

        if !self
            .policy
            .manual_gate_passed(&input.changed_paths, input.manual_approved)
        {
            eval.accepted = false;
            eval.reasons.push("safety_gate_blocked".to_string());
        }

        let elapsed_ms = started_at.elapsed().as_millis();
        if !self.policy.time_allowed(elapsed_ms) {
            eval.accepted = false;
            eval.reasons.push(format!(
                "time_budget_exceeded:{}>{}",
                elapsed_ms, self.policy.time_budget_ms
            ));
        }

        let verdict = if eval.accepted {
            stages.push(IterationStage::Promote);
            self.metrics.iteration_promote_total += 1;
            ChangeVerdict::Promote
        } else {
            stages.push(IterationStage::Rollback);
            self.metrics.iteration_rollback_total += 1;
            ChangeVerdict::Rollback
        };

        self.memory.record(
            input.issue_id,
            input.patch_id,
            verdict,
            eval.quality_score,
        );

        IterationReport {
            stages,
            verdict,
            evaluation: eval,
            elapsed_ms,
        }
    }
}
