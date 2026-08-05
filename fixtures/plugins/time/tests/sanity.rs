//! Sentinel test for the E2E `iterate_plugins` demo.
//!
//! Loads the `time` dylib and calls `time_now`; asserts the returned
//! `timestamp` field is a plausible current-year value. If the plugin
//! is quietly broken (e.g. hardcoded to 0 or negative), this test
//! fails — driving the verifier stage of `iterate_plugins` and
//! forcing the agent to fix the bug before promotion.

use serde_json::Value;

#[test]
fn time_now_returns_positive_current_year_timestamp() {
    // We don't have the plugin API surface easily testable from an
    // integration test without a real host, so we exercise the pure
    // computation path via a shared helper. Since `handle_time_now`
    // is private, we re-implement the sanity property here: the plugin
    // must produce a `timestamp` field > 0 AND within a plausible
    // current-year band.
    //
    // The trick: we call the plugin's `handle` entry via the public
    // `api_handle` (exposed by `export_plugin_api!`). But that requires
    // dlopen. Simpler: assert on the shared reference — SystemTime::now
    // gives us the ground truth; the plugin must produce a value close
    // to it. We check the plugin's public behaviour by reading its
    // `serialize_test_reference()` if available; here we just guard the
    // static bracket 2020..2100.
    let ref_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is forward")
        .as_secs() as i64;

    // Force the plugin to be linked in and produce a response.
    let req = serde_json::json!({
        "node_id": "time_now"
    });
    let response_json = time::__test_call_handle(req.to_string());
    let response: Value = serde_json::from_str(&response_json).expect("plugin returned valid JSON");

    let ts = response
        .get("timestamp")
        .and_then(Value::as_i64)
        .expect("plugin response must include an integer `timestamp`");

    assert!(ts > 0, "time_now timestamp must be positive, got {ts}");
    // Allow +/- 1 day drift vs. our reference clock.
    let delta = (ts - ref_now).abs();
    assert!(
        delta < 86_400,
        "time_now timestamp {ts} not within ±1d of reference {ref_now}"
    );
}
