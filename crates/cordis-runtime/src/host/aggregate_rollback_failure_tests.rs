use super::aggregate_rollback_failure;
use super::validated_verification_command;
use crate::core::error::RuntimeError;

fn err(message: &str) -> RuntimeError {
    RuntimeError::InvalidArgument {
        message: message.to_string(),
    }
}

#[test]
fn validated_verification_command_missing_command_errors_byte_exact() {
    // Both explicit and fallback absent → InvalidArgument with the
    // "missing verification command for <prefix>" wording (line 5298).
    let out = validated_verification_command(None, None, "cargo test");
    assert!(
        matches!(&out, Err(RuntimeError::InvalidArgument { message })
                if *message == "missing verification command for cargo test"),
        "expected missing-command InvalidArgument, got {out:?}"
    );
}

#[test]
fn validated_verification_command_prefix_mismatch_errors_byte_exact() {
    // Explicit command that does not start with the required prefix and is
    // not a recognized short alias → prefix-mismatch InvalidArgument
    // (lines 5309-5314). `rm -rf` never matches `cargo build`.
    let out = validated_verification_command(Some("rm -rf /".to_string()), None, "cargo build");
    assert!(
        matches!(&out, Err(RuntimeError::InvalidArgument { message })
                if *message == "verification tool only allows commands starting with `cargo build`, got `rm -rf /`"),
        "expected prefix-mismatch InvalidArgument, got {out:?}"
    );
}

#[test]
fn no_rollback_errors_returns_base_message_verbatim() {
    // No candidate staged, workspace restore succeeded → base unchanged.
    let out = aggregate_rollback_failure("promote failed: boom".to_string(), None, Ok(()));
    assert_eq!(out, "promote failed: boom");
}

#[test]
fn candidate_ok_and_restore_ok_returns_base_message() {
    let out = aggregate_rollback_failure("stage error".to_string(), Some(Ok(())), Ok(()));
    assert_eq!(out, "stage error");
}

#[test]
fn candidate_rollback_error_only() {
    let out = aggregate_rollback_failure(
        "promote failed: boom".to_string(),
        Some(Err(err("cand kaput"))),
        Ok(()),
    );
    assert_eq!(
        out,
        "promote failed: boom; rollback errors: [candidate rollback: invalid argument: cand kaput]"
    );
}

#[test]
fn workspace_restore_error_only() {
    let out = aggregate_rollback_failure(
        "promote failed: boom".to_string(),
        Some(Ok(())),
        Err(err("restore kaput")),
    );
    assert_eq!(
            out,
            "promote failed: boom; rollback errors: [workspace restore: invalid argument: restore kaput]"
        );
}

#[test]
fn no_candidate_but_workspace_restore_error() {
    // stage_error early-return shape: candidate absent, restore failed.
    let out =
        aggregate_rollback_failure("stage error".to_string(), None, Err(err("restore kaput")));
    assert_eq!(
        out,
        "stage error; rollback errors: [workspace restore: invalid argument: restore kaput]"
    );
}

#[test]
fn both_candidate_and_restore_errors_are_ordered_and_comma_joined() {
    // Candidate rollback error precedes workspace restore error, joined
    // with ", " — must match the historical push order byte-for-byte.
    let out = aggregate_rollback_failure(
        "promote (manual-approved) failed: boom".to_string(),
        Some(Err(err("cand kaput"))),
        Err(err("restore kaput")),
    );
    assert_eq!(
            out,
            "promote (manual-approved) failed: boom; rollback errors: \
             [candidate rollback: invalid argument: cand kaput, workspace restore: invalid argument: restore kaput]"
        );
}
