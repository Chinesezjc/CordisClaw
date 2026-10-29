use super::{host_io_error, soul_reply_error};
use crate::core::error::RuntimeError;
use std::path::PathBuf;

#[test]
fn io_ctx_appends_source_error_to_context() {
    let err = super::io_ctx(
        std::path::PathBuf::from("/x"),
        "failed to write Cargo.toml",
        "disk full",
    );
    assert!(
        matches!(&err, crate::core::error::RuntimeError::Io { path, message }
                if path == std::path::Path::new("/x") && message == "failed to write Cargo.toml: disk full"),
        "got: {err:?}"
    );
}

#[test]
fn flock_failure_line_names_path_and_errno() {
    let line = super::flock_failure_line(std::path::Path::new("/tmp/x.lock"));
    assert!(line.starts_with("[create_plugin] flock(/tmp/x.lock) failed: "));
}

#[test]
fn missing_summary_error_names_the_session() {
    let err = super::missing_summary_error("sess-9");
    assert!(
        matches!(&err, crate::core::error::RuntimeError::LlmResponseInvalid { message }
                if message == "plugin iteration agent session sess-9 exited without calling record_iteration_summary"),
        "got: {err:?}"
    );
}

#[test]
fn host_io_error_carries_path_and_message_verbatim() {
    let path = PathBuf::from("/root/plugins/foo/src");
    let err = host_io_error(
        path.clone(),
        "failed to create plugin src dir: boom".to_string(),
    );
    assert!(
        matches!(&err, RuntimeError::Io { path: p, message } if p == &path && message == "failed to create plugin src dir: boom"),
        "expected Io, got {err:?}"
    );
}

#[test]
fn host_io_error_matches_plain_tostring_form() {
    // Mirrors the snapshot-root create_dir_all closure where message = e.to_string().
    let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    let path = PathBuf::from("/snap/root");
    let err = host_io_error(path.clone(), io.to_string());
    assert!(
        matches!(&err, RuntimeError::Io { path: p, message } if p == &path && *message == io.to_string()),
        "expected Io, got {err:?}"
    );
}

#[test]
fn soul_reply_error_not_json_stage_text_is_byte_exact() {
    let json_err = serde_json::from_str::<serde_json::Value>("{not json").unwrap_err();
    let err = soul_reply_error("/soul/plugin", "is not JSON", &json_err);
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message } if *message == format!("soul_get reply from /soul/plugin is not JSON: {json_err}")),
        "expected InvalidArgument, got {err:?}"
    );
}

#[test]
fn soul_reply_error_malformed_stage_text_is_byte_exact() {
    let val_err = serde_json::from_value::<u32>(serde_json::json!("not a number")).unwrap_err();
    let err = soul_reply_error("/soul/plugin", "malformed", &val_err);
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message } if *message == format!("soul_get reply from /soul/plugin malformed: {val_err}")),
        "expected InvalidArgument, got {err:?}"
    );
}
