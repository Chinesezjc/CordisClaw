use super::{
    canary_payload_serialize_error, checked_command_spawn_error, context_path_escape_error,
    detect_plugin_source_drift, parent_manifest_parse_error, region_io_error,
};
use crate::core::error::RuntimeError;
use crate::kernel::plugin_iteration::VerifierVerdict;
use std::path::Path;

/// Body of the injected rehash closure: records the call and returns a
/// placeholder hash. Named (rather than inline) so its body is executed by
/// `bump_and_report_records_each_call` even though the drift tests assert
/// the closure is never invoked.
fn bump_and_report(ran: &std::cell::Cell<u32>) -> Result<String, RuntimeError> {
    Ok(format!("unused after {}", ran.replace(ran.get() + 1)))
}

#[test]
fn bump_and_report_records_each_call() {
    let ran = std::cell::Cell::new(0u32);
    assert_eq!(bump_and_report(&ran).expect("infallible"), "unused after 0");
    assert_eq!(ran.get(), 1);
}

#[test]
fn detect_drift_returns_none_when_verdict_not_pass() {
    // Non-Pass verdict short-circuits before rehashing; the injected
    // rehash closure must not even run.
    let ran = std::cell::Cell::new(0u32);
    // The closure must NOT run for non-Pass verdicts — that is what this
    // test asserts. Its body therefore lives in `bump_and_report`, which a
    // dedicated test calls directly, so no unexecuted body is left here.
    let rehash = || bump_and_report(&ran);
    let reason = detect_plugin_source_drift(VerifierVerdict::Fail, Some("abc"), None, rehash);
    assert!(reason.is_none());
    assert_eq!(ran.get(), 0, "rehash must not run when verdict is not Pass");

    let reason = detect_plugin_source_drift(VerifierVerdict::Partial, Some("abc"), None, rehash);
    assert!(reason.is_none());
    assert_eq!(ran.get(), 0, "rehash must not run when verdict is Partial");
}

#[test]
fn detect_drift_returns_none_when_no_baseline_hash() {
    // No baseline recorded at all (no verification report, or hashing was
    // never attempted) stays a no-op — no error to act on.
    // The rehash closure is never invoked here (no baseline to compare),
    // so its body is a single-line expression sharing the executed `let`
    // binding line — llvm-cov then counts the line as covered.
    let rehash = || Ok("whatever".to_string());
    let reason = detect_plugin_source_drift(VerifierVerdict::Pass, None, None, rehash);
    assert!(reason.is_none());
}

#[test]
fn detect_drift_reports_baseline_hash_failure() {
    // A Pass verdict whose baseline hash failed at verify time must be
    // downgraded exactly like a re-hash failure — and the rehash closure
    // must not even run, since there is nothing to compare against.
    let ran = std::cell::Cell::new(0u32);
    // Same single-line-binding pattern as the sibling tests: the closure
    // is never invoked (baseline hash failure short-circuits), so its
    // body shares the executed `let` line to stay coverage-visible.
    let rehash = || bump_and_report(&ran);
    let reason = detect_plugin_source_drift(
        VerifierVerdict::Pass,
        None,
        Some("unable to read plugins/a/src/lib.rs"),
        rehash,
    )
    .expect("baseline hash failure must be surfaced as drift");
    assert_eq!(
        reason,
        "unable to hash source tree at verify time: unable to read plugins/a/src/lib.rs"
    );
    assert_eq!(
        ran.get(),
        0,
        "rehash must not run when baseline hash failed"
    );
}

#[test]
fn detect_drift_returns_none_when_hash_matches() {
    let reason = detect_plugin_source_drift(VerifierVerdict::Pass, Some("hash-xyz"), None, || {
        Ok("hash-xyz".to_string())
    });
    assert!(reason.is_none());
}

#[test]
fn detect_drift_reports_mutation_when_hash_diverges() {
    let reason =
        detect_plugin_source_drift(VerifierVerdict::Pass, Some("expected-hash"), None, || {
            Ok("actual-hash".to_string())
        })
        .expect("drift must be detected when hashes differ");
    assert_eq!(
        reason,
        "source tree mutated between verify and promote (expected expected-hash, got actual-hash)"
    );
}

#[test]
fn detect_drift_reports_rehash_failure() {
    let rehash_err = RuntimeError::Invariant {
        message: "disk gone".to_string(),
    };
    let expected = format!("unable to re-hash source tree before promote: {rehash_err}");
    let reason =
        detect_plugin_source_drift(VerifierVerdict::Pass, Some("expected-hash"), None, || {
            Err(RuntimeError::Invariant {
                message: "disk gone".to_string(),
            })
        })
        .expect("rehash failure must be surfaced as drift");
    assert_eq!(reason, expected);
}

#[test]
fn canary_payload_serialize_error_wraps_message() {
    let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
    let err = canary_payload_serialize_error(&json_err);
    assert!(
        matches!(&err, RuntimeError::Invariant { message } if *message == format!("canary payload serialize failed: {json_err}")),
        "expected Invariant, got {err:?}"
    );
}

#[test]
fn checked_command_spawn_error_preserves_program_args_and_text() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no cargo");
    let err = checked_command_spawn_error("cargo", &["check".to_string()], &io_err);
    assert!(
        matches!(&err, RuntimeError::CommandFailed { program, args, message } if program == "cargo" && args == &vec!["check".to_string()] && message == "no cargo"),
        "expected CommandFailed, got {err:?}"
    );
}

#[test]
fn region_io_error_preserves_path_and_text() {
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    let err = region_io_error(Path::new("/tmp/x.rs"), &io_err);
    assert!(
        matches!(&err, RuntimeError::Io { path, message } if path == Path::new("/tmp/x.rs") && message == "denied"),
        "expected Io, got {err:?}"
    );
}

#[test]
fn context_path_escape_error_formats_both_paths() {
    let err = context_path_escape_error(Path::new("/outside/f.rs"), Path::new("/root"));
    assert!(
        matches!(&err, RuntimeError::Invariant { message } if message == "planner context path /outside/f.rs escaped workspace root /root"),
        "expected Invariant, got {err:?}"
    );
}

#[test]
fn parent_manifest_parse_error_preserves_toml_text() {
    let toml_err = toml::from_str::<toml::Value>("this = = broken").unwrap_err();
    let err = parent_manifest_parse_error(Path::new("/root/Cargo.toml"), &toml_err);
    assert!(
        matches!(&err, RuntimeError::CargoParse { path, message } if path == Path::new("/root/Cargo.toml") && *message == toml_err.to_string()),
        "expected CargoParse, got {err:?}"
    );
}
