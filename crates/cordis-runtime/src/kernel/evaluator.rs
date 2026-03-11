//! Verification and scoring harness for self-iteration.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationInput {
    pub tests_passed: bool,
    pub safety_checks_passed: bool,
    pub quality_score: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationReport {
    pub accepted: bool,
    pub tests_passed: bool,
    pub safety_checks_passed: bool,
    pub quality_score: u32,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalHarness {
    pub min_quality_score: u32,
}

impl Default for EvalHarness {
    fn default() -> Self {
        Self {
            min_quality_score: 80,
        }
    }
}

impl EvalHarness {
    /// Evaluate verification outputs into a single promote/rollback decision.
    pub fn evaluate(&self, input: VerificationInput) -> EvaluationReport {
        let mut reasons = Vec::new();
        if !input.tests_passed {
            reasons.push("tests_failed".to_string());
        }
        if !input.safety_checks_passed {
            reasons.push("safety_checks_failed".to_string());
        }
        if input.quality_score < self.min_quality_score {
            reasons.push(format!(
                "quality_score_too_low:{}<{}",
                input.quality_score, self.min_quality_score
            ));
        }

        EvaluationReport {
            accepted: reasons.is_empty(),
            tests_passed: input.tests_passed,
            safety_checks_passed: input.safety_checks_passed,
            quality_score: input.quality_score,
            reasons,
        }
    }
}
