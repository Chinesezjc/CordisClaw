//! The kernel injects every plugin's `system_hint` into EVERY session, whatever
//! channel that session belongs to. Feishu's hint is phrased as an
//! unconditional global command ("Your FINAL output each turn MUST be exactly
//! one JSON object … do NOT call feishu_send yourself"), so in a QQ group turn
//! the model followed it: one plain line of chat text, no tool call, and the
//! whole turn was discarded as `no_tool_call`.
//!
//! QQ cannot rewrite the other plugin's hint (and the runtime is not ours to
//! change), but its own card is injected in the same breath and the tail of a
//! hint is its strongest part — so the QQ card must NAME that foreign rule and
//! say it does not apply here. Without this, the channel card and the Feishu
//! card contradict each other and the model has to guess.
//!
//! These assertions read the generated `docs/agent/interfaces.json`, i.e. what
//! the kernel actually ships, not the Rust literal.

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

// Another channel's end-of-turn rule must not be obeyed here: the hint has to
// admit that foreign cards exist and tell the model to ignore them.
#[test]
fn hint_says_other_channels_rules_do_not_apply_here() {
    let hint = system_hint();
    for required in ["别的渠道", "飞书", "忽略"] {
        assert!(
            hint.contains(required),
            "the QQ card must name other channels' rules and tell the model to ignore them; \
             missing {required:?} in:\n{hint}"
        );
    }
}

// The rule being overridden is specifically "answer with one bare JSON object",
// which is what turned a QQ turn into `no_tool_call`.
#[test]
fn the_warning_names_the_json_object_rule_it_overrides() {
    let hint = system_hint();
    assert!(
        hint.contains("JSON"),
        "the warning must name the JSON-object rule it overrides:\n{hint}"
    );
    assert!(
        hint.contains("qq_send") && hint.contains("suspend"),
        "the warning must point at the QQ way out (qq_send + suspend):\n{hint}"
    );
}

// The card is injected into EVERY session, so it must say which channel it is
// talking about *before* it forbids anything. A Feishu session that reads only
// "no JSON, no tool call" drops its envelope and the turn dies as
// `no_tool_call`.
#[test]
fn hint_scopes_itself_by_channel() {
    let hint = system_hint();
    for required in ["哪个渠道", "飞书会话", "feishu_send", "respond"] {
        assert!(
            hint.contains(required),
            "the QQ card must first identify the channel; missing {required:?} in:\n{hint}"
        );
    }
}

// "Do not output JSON" is a QQ-session rule, not a global imperative: the final
// override paragraph has to name both channels so a Feishu turn keeps its
// envelope.
#[test]
fn qq_only_instructions_are_marked_as_qq_only() {
    let hint = system_hint();
    let override_at = hint
        .find("别的渠道")
        .unwrap_or_else(|| panic!("the override is missing:\n{hint}"));
    let tail = &hint[override_at..];
    for required in ["飞书会话", "QQ 会话"] {
        assert!(
            tail.contains(required),
            "the final override paragraph must scope the JSON rule by channel; \
             missing {required:?} in:\n{tail}"
        );
    }
}

// Last word wins: the override has to come after the send/end-of-turn rules it
// corrects, never before them.
#[test]
fn the_warning_is_the_last_word() {
    let hint = system_hint();
    let send_rule = hint
        .find("【发言必须走工具】")
        .unwrap_or_else(|| panic!("the send rule is missing:\n{hint}"));
    let override_at = hint
        .find("别的渠道")
        .unwrap_or_else(|| panic!("the override is missing:\n{hint}"));
    assert!(
        send_rule < override_at,
        "the cross-channel override must come after the send rule \
         (send rule at {send_rule}, override at {override_at}):\n{hint}"
    );
}
