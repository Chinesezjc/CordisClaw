//! The plugin `system_hint` is injected by the kernel into EVERY session, not
//! just Feishu ones. So the hint must scope itself by channel: in a QQ session
//! (inbound `[QQ ...]`, speaking goes through `qq_send`) the Feishu reply
//! envelope must be explicitly declared inapplicable — otherwise the model
//! reads "your final output MUST be one JSON object" as a global rule, answers
//! with one bare line of prose and no tool call, and the runtime drops the
//! whole turn as `no_tool_call`.
//!
//! These tests read the generated `docs/agent/interfaces.json` (the same file
//! the runtime consumes) rather than the source, so a hint edited in
//! `docs_value()` but not regenerated is caught too.

use serde_json::Value;

/// The generated `system_hint` of the feishu plugin.
fn system_hint() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/agent/interfaces.json");
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read generated plugin docs at {path}: {e}"));
    let docs: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("generated plugin docs at {path} are not valid JSON: {e}"));
    docs.get("system_hint")
        .and_then(|hint| hint.as_str())
        .unwrap_or_else(|| panic!("generated plugin docs at {path} must carry a system_hint"))
        .to_string()
}

// 这条 hint 会进每一个会话:必须先按渠道自我限定，再谈规则。
#[test]
fn hint_scopes_itself_by_channel_before_any_rule() {
    let hint = system_hint();

    // The channel check markers: "which channel", "Feishu session", and the
    // other channel's own send tool + its bail-out action.
    for marker in ["哪个渠道", "飞书会话", "qq_send", "suspend"] {
        assert!(
            hint.contains(marker),
            "system_hint must scope itself by channel; missing {marker:?} in: {hint}"
        );
    }

    // It must say the JSON rule does NOT apply to a non-Feishu session.
    assert!(
        hint.contains("不适用"),
        "system_hint must state the JSON rule is inapplicable outside Feishu, got: {hint}"
    );

    // And the channel check must come FIRST, before the reply contract.
    let scope_at = hint
        .find("哪个渠道")
        .expect("the channel check marker must be present");
    let contract_at = hint
        .find("{\"action\":\"respond\"")
        .expect("the respond envelope must be present");
    assert!(
        scope_at < contract_at,
        "the channel check must precede the reply rule, got: {hint}"
    );
}

// 旧 hint 说 \"do NOT call feishu_send yourself\"，与工具卡自相矛盾：飞书整条
// 通道都靠模型在轮内调 feishu_send / 图片 / fetch。
#[test]
fn hint_does_not_forbid_the_feishu_send_tool() {
    let hint = system_hint();

    assert!(
        !hint.contains("do NOT call feishu_send yourself"),
        "system_hint must not forbid feishu_send, got: {hint}"
    );
    assert!(
        hint.contains("Calling feishu_send"),
        "system_hint must name feishu_send as callable mid-turn, got: {hint}"
    );
    assert!(
        hint.contains("is allowed and expected"),
        "system_hint must say calling the send/fetch tools mid-turn is allowed, got: {hint}"
    );
    for tool in ["feishu_send", "feishu_send_image", "feishu_fetch"] {
        assert!(
            hint.contains(tool),
            "system_hint must keep {tool} available mid-turn, got: {hint}"
        );
    }
}

// 收尾契约本身不能被削弱：respond / suspend，以及只写正文会被判 no_tool_call。
#[test]
fn hint_still_states_the_respond_suspend_contract() {
    let hint = system_hint();

    for needle in ["respond", "suspend", "只写正文", "no_tool_call"] {
        assert!(
            hint.contains(needle),
            "system_hint must still state {needle:?}, got: {hint}"
        );
    }
    // Both envelope shapes are named explicitly.
    assert!(
        hint.contains("\"action\":\"respond\""),
        "the respond envelope must stay in the hint, got: {hint}"
    );
    assert!(
        hint.contains("\"action\":\"suspend\""),
        "the suspend envelope must stay in the hint, got: {hint}"
    );
}
