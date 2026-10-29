use super::{
    plugin_iteration_missing_rollback_error, plugin_iteration_panic_error,
    verdict_rollback_partial_cleanup_reason,
};
use crate::core::error::RuntimeError;

/// Assert `err` is an `Invariant` carrying exactly `expected`. Compares the
/// whole error value rather than destructuring it, so there is no
/// unreachable `else`-panic arm for llvm-cov to report.
fn assert_invariant(err: &RuntimeError, expected: &str) {
    assert_eq!(
        err.to_string(),
        format!("internal invariant broken: {expected}")
    );
}

/// Step 2's `None`-rollback arm. `run_plugin_iteration_agent` returns a
/// rollback on both of its branches — the edit-plan branch starts from
/// `PluginEditRollback::empty(..)` and the agent branch reads it out of the
/// session snapshot — so `state.rollback` is `Some` for every
/// `iterate_plugins` call. The error stays as a named constructor so its
/// wording is pinned.
#[test]
fn missing_rollback_error_carries_the_pinned_invariant_message() {
    assert_invariant(
        &plugin_iteration_missing_rollback_error(),
        "plugin iteration rollback journal missing after agent execution",
    );
}

/// `panic!("literal")` hands `catch_unwind` a `&'static str` payload.
#[test]
fn panic_error_renders_a_str_payload() {
    let payload: Box<dyn std::any::Any + Send> = Box::new("boom from a literal");
    assert_invariant(
            &plugin_iteration_panic_error(&payload),
            "plugin iteration panicked at an unexpected point; workspace has been restored: boom from a literal",
        );
}

/// A formatted `panic!`/`assert!` message arrives as a `String`.
#[test]
fn panic_error_renders_a_string_payload() {
    let payload: Box<dyn std::any::Any + Send> = Box::new("boom from a format".to_string());
    assert_invariant(
            &plugin_iteration_panic_error(&payload),
            "plugin iteration panicked at an unexpected point; workspace has been restored: boom from a format",
        );
}

/// Any other payload type is only producible by `panic_any`, which the
/// iteration body never calls — hence the fixed fallback label.
#[test]
fn panic_error_falls_back_for_an_unknown_payload_type() {
    let payload: Box<dyn std::any::Any + Send> = Box::new(42_u32);
    assert_invariant(
            &plugin_iteration_panic_error(&payload),
            "plugin iteration panicked at an unexpected point; workspace has been restored: unknown panic payload",
        );
}

/// finalize's negative-verdict arm when the candidate rollback *also*
/// fails. Reaching it needs a staged candidate whose `rollback_candidate`
/// errors while the subsequent workspace restore succeeds — the restore
/// runs the same `restore_plugin_iteration_workspace` the failing rollback
/// already tried, so any fault that breaks one breaks the other and the
/// `?` fires first. The message is pinned here instead.
#[test]
fn verdict_rollback_partial_cleanup_reason_nests_both_prefixes() {
    let reason = verdict_rollback_partial_cleanup_reason(&RuntimeError::Invariant {
        message: "candidate went missing".to_string(),
    });
    assert_eq!(
            reason,
            "verdict rollback with partial candidate cleanup error: candidate rollback: internal invariant broken: candidate went missing"
        );
}
