//! The kernel injects every plugin's `system_hint` into EVERY session, whatever
//! channel that session belongs to. So the QQ card's "do not output JSON" clause
//! cannot read as a global imperative: a Feishu session that obeys it drops its
//! `{"action":"respond",…}` envelope, answers with bare chat text, and the whole
//! turn is discarded as `no_tool_call`.
//!
//! The fix lives in the card's wording, not in the runtime: the QQ hint must
//! first name the channel check (and the Feishu way out), and its final override
//! paragraph must scope the no-JSON rule to QQ sessions while spelling out the
//! Feishu side. These assertions read the generated `docs/agent/interfaces.json`
//! — what the kernel actually ships — not the Rust literal.

use std::fs;
use std::path::PathBuf;

fn system_hint() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/agent/interfaces.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let json: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    json.get("system_hint")
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| panic!("qq must ship a system_hint in {}", path.display()))
        .to_string()
}

// The card is read by a Feishu session too, so before it forbids anything it has
// to say which channel it is talking about and what the Feishu way out is.
#[test]
fn qq_card_tells_a_feishu_session_to_keep_its_envelope() {
    let hint = system_hint();
    for required in ["哪个渠道", "飞书会话", "feishu_send", "respond"] {
        assert!(
            hint.contains(required),
            "the QQ card must name the channel check and the Feishu way out; \
             missing {required:?} in:\n{hint}"
        );
    }
}

// "Do not output JSON" is a QQ-session rule, not a global one: the final
// override paragraph has to name both channels, or a Feishu turn drops its
// envelope and dies as `no_tool_call`.
#[test]
fn qq_card_scopes_the_no_json_rule_to_qq_sessions() {
    let hint = system_hint();
    let override_at = hint
        .find("别的渠道")
        .unwrap_or_else(|| panic!("the cross-channel override is missing:\n{hint}"));
    let tail = &hint[override_at..];
    for required in ["飞书会话", "QQ 会话"] {
        assert!(
            tail.contains(required),
            "the final override paragraph must scope the no-JSON rule by channel; \
             missing {required:?} in:\n{tail}"
        );
    }
}
