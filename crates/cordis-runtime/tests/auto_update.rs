use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::kernel::auto_update::{AutoUpdatePlan, AutoUpdater, FilePatch};
use cordis_runtime::kernel::evaluator::{EvalHarness, VerificationInput};
use cordis_runtime::kernel::memory::ChangeVerdict;
use cordis_runtime::kernel::policy::IterationPolicy;
use cordis_runtime::kernel::r#loop::SelfIterationKernel;
use std::fs;
use tempfile::TempDir;

fn make_kernel() -> SelfIterationKernel {
    let policy = IterationPolicy {
        path_allowlist: vec!["".to_string()],
        sensitive_path_prefixes: vec![],
        require_manual_approval_for_sensitive: false,
        max_diff_lines: 500,
        time_budget_ms: 60_000,
    };
    SelfIterationKernel::new(
        policy,
        EvalHarness {
            min_quality_score: 80,
        },
        Default::default(),
    )
}

#[test]
fn auto_update_apply_and_promote_keeps_changes() {
    let temp = TempDir::new().expect("tempdir");
    let file = temp.path().join("demo.txt");
    fs::write(&file, "alpha-old-omega").expect("write demo");

    let mut kernel = make_kernel();
    let updater = AutoUpdater::new(temp.path());
    let result = updater
        .execute(
            &mut kernel,
            AutoUpdatePlan {
                issue_id: "issue-1".to_string(),
                patch_id: "patch-1".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch {
                    path: "demo.txt".to_string(),
                    find: "old".to_string(),
                    replace: "new".to_string(),
                }],
            },
            |_| {
                Ok(VerificationInput {
                    tests_passed: true,
                    safety_checks_passed: true,
                    quality_score: 95,
                })
            },
        )
        .expect("auto update should succeed");

    assert_eq!(result.report.verdict, ChangeVerdict::Promote);
    assert!(!result.rolled_back);
    let content = fs::read_to_string(&file).expect("read demo");
    assert_eq!(content, "alpha-new-omega");
}

#[test]
fn auto_update_verify_failure_rolls_back_changes() {
    let temp = TempDir::new().expect("tempdir");
    let file = temp.path().join("demo.txt");
    fs::write(&file, "alpha-old-omega").expect("write demo");

    let mut kernel = make_kernel();
    let updater = AutoUpdater::new(temp.path());
    let result = updater
        .execute(
            &mut kernel,
            AutoUpdatePlan {
                issue_id: "issue-2".to_string(),
                patch_id: "patch-2".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch {
                    path: "demo.txt".to_string(),
                    find: "old".to_string(),
                    replace: "new".to_string(),
                }],
            },
            |_| {
                Ok(VerificationInput {
                    tests_passed: false,
                    safety_checks_passed: true,
                    quality_score: 95,
                })
            },
        )
        .expect("auto update should complete with rollback verdict");

    assert_eq!(result.report.verdict, ChangeVerdict::Rollback);
    assert!(result.rolled_back);
    let content = fs::read_to_string(&file).expect("read demo");
    assert_eq!(content, "alpha-old-omega");
}

#[test]
fn auto_update_rejects_parent_dir_traversal() {
    let temp = TempDir::new().expect("tempdir");
    let mut kernel = make_kernel();
    let updater = AutoUpdater::new(temp.path());
    let err = updater
        .execute(
            &mut kernel,
            AutoUpdatePlan {
                issue_id: "issue-3".to_string(),
                patch_id: "patch-3".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch {
                    path: "../escape.txt".to_string(),
                    find: "a".to_string(),
                    replace: "b".to_string(),
                }],
            },
            |_| {
                Ok(VerificationInput {
                    tests_passed: true,
                    safety_checks_passed: true,
                    quality_score: 95,
                })
            },
        )
        .expect_err("traversal must be rejected");

    assert!(matches!(err, RuntimeError::AutoUpdateInvalidPath { .. }));
}
