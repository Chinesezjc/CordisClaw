//! Verification and scoring harness for self-iteration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationInput {
    pub tests_passed: bool,
    pub safety_checks_passed: bool,
    pub quality_score: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::{EvalHarness, VerificationInput};

    fn input(tests: bool, safety: bool, score: u32) -> VerificationInput {
        VerificationInput {
            tests_passed: tests,
            safety_checks_passed: safety,
            quality_score: score,
        }
    }

    #[test]
    fn default_min_quality_score() {
        assert_eq!(EvalHarness::default().min_quality_score, 80);
    }

    #[test]
    fn accepts_when_all_conditions_pass() {
        let report = EvalHarness::default().evaluate(input(true, true, 80));
        assert!(report.accepted);
        assert!(report.reasons.is_empty());
        assert!(report.tests_passed);
        assert!(report.safety_checks_passed);
        assert_eq!(report.quality_score, 80);
    }

    #[test]
    fn rejects_when_tests_fail() {
        let report = EvalHarness::default().evaluate(input(false, true, 100));
        assert!(!report.accepted);
        assert!(report.reasons.contains(&"tests_failed".to_string()));
    }

    #[test]
    fn rejects_when_safety_fails() {
        let report = EvalHarness::default().evaluate(input(true, false, 100));
        assert!(!report.accepted);
        assert!(report.reasons.contains(&"safety_checks_failed".to_string()));
    }

    #[test]
    fn rejects_and_formats_low_quality_reason() {
        let report = EvalHarness::default().evaluate(input(true, true, 79));
        assert!(!report.accepted);
        assert_eq!(report.reasons, vec!["quality_score_too_low:79<80"]);
    }

    #[test]
    fn accumulates_all_failure_reasons() {
        let report = EvalHarness::default().evaluate(input(false, false, 10));
        assert!(!report.accepted);
        assert_eq!(report.reasons.len(), 3);
        assert_eq!(report.reasons[0], "tests_failed");
        assert_eq!(report.reasons[1], "safety_checks_failed");
        assert_eq!(report.reasons[2], "quality_score_too_low:10<80");
    }

    #[test]
    fn custom_threshold_is_honored() {
        let harness = EvalHarness {
            min_quality_score: 50,
        };
        // 60 clears a threshold of 50 but would fail the default 80.
        let report = harness.evaluate(input(true, true, 60));
        assert!(report.accepted);
    }
}
