use cordis_runtime::kernel::evaluator::{EvalHarness, VerificationInput};
use cordis_runtime::kernel::memory::{ChangeMemory, ChangeVerdict};
use cordis_runtime::kernel::policy::IterationPolicy;
use cordis_runtime::kernel::r#loop::{IterationInput, IterationStage, SelfIterationKernel};

#[test]
fn kernel_promotes_when_policy_and_evaluation_pass() {
    let policy = IterationPolicy::default();
    let evaluator = EvalHarness::default();
    let memory = ChangeMemory::default();
    let mut kernel = SelfIterationKernel::new(policy, evaluator, memory);

    let report = kernel.run_once(IterationInput {
        issue_id: "issue-1".to_string(),
        patch_id: "patch-1".to_string(),
        patch_kind: "text".to_string(),
        changed_paths: vec!["crates/cordis-runtime/src/execution/engine.rs".to_string()],
        diff_lines: 120,
        manual_approved: false,
        verification_profile: Some("default".to_string()),
        verification: VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 90,
        },
    });

    assert_eq!(report.verdict, ChangeVerdict::Promote);
    assert_eq!(report.stages.last(), Some(&IterationStage::Promote));
    assert!(report.evaluation.accepted);
    assert_eq!(kernel.memory().len(), 1);
    assert_eq!(kernel.metrics().iteration_total, 1);
    assert_eq!(kernel.metrics().iteration_promote_total, 1);
    assert_eq!(kernel.metrics().iteration_rollback_total, 0);
}

#[test]
fn kernel_rolls_back_on_path_violation() {
    let policy = IterationPolicy {
        path_allowlist: vec!["crates/".to_string()],
        sensitive_path_prefixes: vec!["crates/cordis-runtime/src/core/".to_string()],
        require_manual_approval_for_sensitive: true,
        max_diff_lines: 200,
        time_budget_ms: 60_000,
    };
    let evaluator = EvalHarness::default();
    let memory = ChangeMemory::default();
    let mut kernel = SelfIterationKernel::new(policy, evaluator, memory);

    let report = kernel.run_once(IterationInput {
        issue_id: "issue-2".to_string(),
        patch_id: "patch-2".to_string(),
        patch_kind: "text".to_string(),
        changed_paths: vec!["scripts/unsafe.sh".to_string()],
        diff_lines: 10,
        manual_approved: false,
        verification_profile: Some("default".to_string()),
        verification: VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 95,
        },
    });

    assert_eq!(report.verdict, ChangeVerdict::Rollback);
    assert_eq!(report.stages.last(), Some(&IterationStage::Rollback));
    assert!(
        report.evaluation.reasons.iter().any(|r| r == "path_not_allowed"),
        "expected path_not_allowed reason, got {:?}",
        report.evaluation.reasons
    );
}

#[test]
fn kernel_rolls_back_on_quality_failure() {
    let policy = IterationPolicy::default();
    let evaluator = EvalHarness {
        min_quality_score: 85,
    };
    let memory = ChangeMemory::with_limit(2);
    let mut kernel = SelfIterationKernel::new(policy, evaluator, memory);

    let report = kernel.run_once(IterationInput {
        issue_id: "issue-3".to_string(),
        patch_id: "patch-3".to_string(),
        patch_kind: "text".to_string(),
        changed_paths: vec!["docs/rs-files-responsibility.md".to_string()],
        diff_lines: 30,
        manual_approved: false,
        verification_profile: Some("default".to_string()),
        verification: VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 60,
        },
    });

    assert_eq!(report.verdict, ChangeVerdict::Rollback);
    assert!(
        report
            .evaluation
            .reasons
            .iter()
            .any(|r| r.starts_with("quality_score_too_low")),
        "expected quality failure reason, got {:?}",
        report.evaluation.reasons
    );
    assert_eq!(kernel.memory().len(), 1);
}

#[test]
fn kernel_rolls_back_when_sensitive_change_has_no_manual_approval() {
    let policy = IterationPolicy::default();
    let evaluator = EvalHarness::default();
    let memory = ChangeMemory::default();
    let mut kernel = SelfIterationKernel::new(policy, evaluator, memory);

    let report = kernel.run_once(IterationInput {
        issue_id: "issue-4".to_string(),
        patch_id: "patch-4".to_string(),
        patch_kind: "text".to_string(),
        changed_paths: vec!["crates/cordis-runtime/src/core/error.rs".to_string()],
        diff_lines: 20,
        manual_approved: false,
        verification_profile: Some("default".to_string()),
        verification: VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 90,
        },
    });

    assert_eq!(report.verdict, ChangeVerdict::Rollback);
    assert_eq!(report.stages.last(), Some(&IterationStage::Rollback));
    assert!(
        report
            .evaluation
            .reasons
            .iter()
            .any(|r| r == "safety_gate_blocked"),
        "expected safety gate reason, got {:?}",
        report.evaluation.reasons
    );
    assert_eq!(kernel.metrics().iteration_total, 1);
    assert_eq!(kernel.metrics().iteration_promote_total, 0);
    assert_eq!(kernel.metrics().iteration_rollback_total, 1);
}
