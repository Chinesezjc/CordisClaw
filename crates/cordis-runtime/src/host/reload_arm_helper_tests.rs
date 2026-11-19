use super::{
    log_session_reload_notify_outcome, reload_abi_fingerprint_parse_error,
    reload_artifact_missing_error, reload_docs_parse_error, reload_stop_handler_panic_message,
    RuntimeHost,
};
use crate::core::error::RuntimeError;
use crate::core::models::PluginUnavailableReason;
use std::any::Any;
use std::time::Instant;

fn json_error() -> serde_json::Error {
    serde_json::from_str::<serde_json::Value>("{ not json").expect_err("must not parse")
}

// ── reload_subtree Phase-1 error constructors ────────────────────────

#[test]
fn artifact_missing_error_is_non_required_unavailable() {
    let err = reload_artifact_missing_error("expr/evaluator");
    let expected = RuntimeError::PluginUnavailable {
        plugin_path: "expr/evaluator".to_string(),
        reason: PluginUnavailableReason::ArtifactMissing,
        required: false,
    };
    // Full-value comparison: plugin_path, reason and the `required: false`
    // classification are all load-bearing for the reload report.
    // `Display` renders plugin_path, reason and the `required: false`
    // classification, so this one comparison pins every field without a
    // never-taken destructuring arm.
    assert_eq!(err.to_string(), expected.to_string());
}

#[test]
fn docs_parse_error_names_the_plugin_and_quotes_serde() {
    let source = json_error();
    let err = reload_docs_parse_error("qq", &source);
    let expected = RuntimeError::Invariant {
        message: format!("failed to parse docs for qq: {source}"),
    };
    assert_eq!(err.to_string(), expected.to_string());
}

#[test]
fn abi_fingerprint_parse_error_names_the_plugin_and_quotes_serde() {
    let source = json_error();
    let err = reload_abi_fingerprint_parse_error("feishu", &source);
    let expected = RuntimeError::Invariant {
        message: format!("failed to parse abi_fingerprint for feishu: {source}"),
    };
    assert_eq!(err.to_string(), expected.to_string());
}

// ── stop-handler panic payload rendering ─────────────────────────────

// A literal `panic!("...")` yields a `&'static str` payload — the shape the
// runtime's own injection point produces.
#[test]
fn stop_handler_panic_message_reads_static_str_payload() {
    let payload: Box<dyn Any + Send> = Box::new("literal boom");
    assert_eq!(
        reload_stop_handler_panic_message(payload.as_ref()),
        "literal boom"
    );
}

// A formatted `panic!("{}", x)` yields an owned `String` payload.
#[test]
fn stop_handler_panic_message_reads_owned_string_payload() {
    let payload: Box<dyn Any + Send> = Box::new(String::from("formatted boom"));
    assert_eq!(
        reload_stop_handler_panic_message(payload.as_ref()),
        "formatted boom"
    );
}

// `panic_any(v)` with a non-string `v` falls through to the placeholder
// instead of panicking inside the diagnostic itself.
#[test]
fn stop_handler_panic_message_falls_back_for_non_string_payload() {
    let payload: Box<dyn Any + Send> = Box::new(42u32);
    assert_eq!(
        reload_stop_handler_panic_message(payload.as_ref()),
        "unknown"
    );
}

// The rendering is reached through a real `catch_unwind` of a `panic_any`,
// matching how `reload_subtree` calls it.
#[test]
fn stop_handler_panic_message_renders_a_caught_panic_any() {
    let caught = std::panic::catch_unwind(|| {
        std::panic::panic_any(7i64);
    })
    .expect_err("panic_any must unwind");
    assert_eq!(
        reload_stop_handler_panic_message(caught.as_ref()),
        "unknown"
    );
}

// ── notify_sessions_of_reload per-session outcome ─────────────────────

// The Ok arm is a no-op; the Err arm logs. Both are total, so the assertion
// is that neither panics for either outcome.
#[test]
fn session_notify_outcome_logging_handles_both_arms() {
    log_session_reload_notify_outcome("sid-ok", Ok(()));
    log_session_reload_notify_outcome(
        "sid-gone",
        Err(RuntimeError::AgentSessionNotFound {
            session_id: "sid-gone".to_string(),
        }),
    );
}

// ── RuntimeHost::fail_reload packaging ───────────────────────────────

/// `fail_reload` pairs the error with a Failed attempt whose
/// `failure_summary` is the error text and whose `to_snapshot_id` is unset.
/// Called directly because every `reload_subtree` call site that reaches it
/// needs a distinct artifact fault to synthesize.
#[test]
fn fail_reload_packages_error_with_failed_attempt() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("create artifacts dir");
    std::fs::write(
            artifacts.join("index.json"),
            r#"{"schema_version":2,"generated_at":"2026-07-24T00:00:00Z","topo_order":[],"entries":[]}"#,
        )
        .expect("write empty index");
    let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");

    let snapshot = host.current_snapshot();
    let err = RuntimeError::Invariant {
        message: "synthetic reload fault".to_string(),
    };
    let rendered = err.to_string();
    let (returned, attempt) = host.fail_reload(&snapshot, Instant::now(), err);

    // The error is handed back unchanged, and the attempt's summary is
    // exactly its rendered form.
    assert_eq!(returned.to_string(), rendered);
    assert_eq!(attempt.status, super::ReloadAttemptStatus::Failed);
    assert_eq!(attempt.from_snapshot_id, snapshot.snapshot_id());
    assert_eq!(attempt.to_snapshot_id, None);
    assert_eq!(attempt.failure_summary, Some(rendered));
    assert_eq!(attempt.plugin_count, None);
    assert!(attempt.changed_plugins.is_empty());
}
