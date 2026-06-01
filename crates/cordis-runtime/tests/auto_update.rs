use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::kernel::auto_update::{AutoUpdatePlan, AutoUpdater, FilePatch};
use cordis_runtime::kernel::evaluator::VerificationInput;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

#[test]
fn auto_update_apply_and_promote_keeps_changes() {
    let temp = TempDir::new().expect("tempdir");
    let file = temp.path().join("demo.txt");
    fs::write(&file, "alpha-old-omega").expect("write demo");

    let updater = AutoUpdater::new(temp.path());
    let result = updater
        .execute(
            AutoUpdatePlan {
                issue_id: "issue-1".to_string(),
                patch_id: "patch-1".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::text("demo.txt", "old", "new")],
            },
            |_| {
                Ok(VerificationInput {
                    tests_passed: true,
                    safety_checks_passed: true,
                    quality_score: 95,
                }
                .into())
            },
        )
        .expect("auto update should succeed");

    assert_eq!(result.verdict, "promote");
    assert!(!result.rolled_back);
    let content = fs::read_to_string(&file).expect("read demo");
    assert_eq!(content, "alpha-new-omega");
}

#[test]
fn auto_update_verify_failure_rolls_back_changes() {
    let temp = TempDir::new().expect("tempdir");
    let file = temp.path().join("demo.txt");
    fs::write(&file, "alpha-old-omega").expect("write demo");

    let updater = AutoUpdater::new(temp.path());
    let result = updater
        .execute(
            AutoUpdatePlan {
                issue_id: "issue-2".to_string(),
                patch_id: "patch-2".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::text("demo.txt", "old", "new")],
            },
            |_| {
                Ok(VerificationInput {
                    tests_passed: false,
                    safety_checks_passed: true,
                    quality_score: 95,
                }
                .into())
            },
        )
        .expect("auto update should complete with rollback verdict");

    assert_eq!(result.verdict, "rollback");
    assert!(result.rolled_back);
    let content = fs::read_to_string(&file).expect("read demo");
    assert_eq!(content, "alpha-old-omega");
}

#[test]
fn auto_update_rejects_parent_dir_traversal() {
    let temp = TempDir::new().expect("tempdir");
    let updater = AutoUpdater::new(temp.path());
    let err = updater
        .execute(
            AutoUpdatePlan {
                issue_id: "issue-3".to_string(),
                patch_id: "patch-3".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::text("../escape.txt", "a", "b")],
            },
            |_| {
                Ok(VerificationInput {
                    tests_passed: true,
                    safety_checks_passed: true,
                    quality_score: 95,
                }
                .into())
            },
        )
        .expect_err("traversal must be rejected");

    assert!(matches!(err, RuntimeError::AutoUpdateInvalidPath { .. }));
}

#[test]
fn auto_update_supports_structured_json_patch() {
    let temp = TempDir::new().expect("tempdir");
    let file = temp.path().join("config.json");
    fs::write(&file, "{\n  \"enabled\": false,\n  \"name\": \"demo\"\n}\n").expect("write config");

    let updater = AutoUpdater::new(temp.path());
    let result = updater
        .execute(
            AutoUpdatePlan {
                issue_id: "issue-json".to_string(),
                patch_id: "patch-json".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::json_value(
                    "config.json",
                    "/enabled",
                    json!(true),
                )],
            },
            |_| {
                Ok(VerificationInput {
                    tests_passed: true,
                    safety_checks_passed: true,
                    quality_score: 95,
                }
                .into())
            },
        )
        .expect("json patch should succeed");

    assert_eq!(result.verdict, "promote");
    let content = fs::read_to_string(&file).expect("read config");
    assert!(content.contains("\"enabled\": true"), "content: {content}");
}

#[test]
fn auto_update_supports_structured_toml_patch() {
    let temp = TempDir::new().expect("tempdir");
    let file = temp.path().join("runtime.toml");
    fs::write(
        &file,
        "[runtime]\nenabled = false\n[limits]\nmax_retries = 1\n",
    )
    .expect("write toml");

    let updater = AutoUpdater::new(temp.path());
    let result = updater
        .execute(
            AutoUpdatePlan {
                issue_id: "issue-toml".to_string(),
                patch_id: "patch-toml".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::toml_value(
                    "runtime.toml",
                    "limits.max_retries",
                    json!(3),
                )],
            },
            |_| {
                Ok(VerificationInput {
                    tests_passed: true,
                    safety_checks_passed: true,
                    quality_score: 95,
                }
                .into())
            },
        )
        .expect("toml patch should succeed");

    assert_eq!(result.verdict, "promote");
    let content = fs::read_to_string(&file).expect("read toml");
    assert!(content.contains("max_retries = 3"), "content: {content}");
}
