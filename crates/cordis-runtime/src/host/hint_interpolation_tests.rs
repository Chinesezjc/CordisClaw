use super::interpolate_hint_settings;
use serde_json::json;

#[test]
fn interpolates_scalar_settings() {
    let settings = json!({ "bot_uin": "3832224285", "count": 3, "flag": true });
    assert_eq!(
        interpolate_hint_settings(
            "you are qq user {setting:bot_uin} (#{setting:count})",
            Some(&settings)
        ),
        "you are qq user 3832224285 (#3)"
    );
    assert_eq!(
        interpolate_hint_settings("{setting:flag}", Some(&settings)),
        "true"
    );
}

#[test]
fn leaves_unresolvable_placeholders_verbatim() {
    // Missing key, non-scalar value, no settings block, malformed marker.
    assert_eq!(
        interpolate_hint_settings("id={setting:bot_uin}", Some(&json!({}))),
        "id={setting:bot_uin}"
    );
    assert_eq!(
        interpolate_hint_settings("x={setting:obj}", Some(&json!({ "obj": {"a": 1} }))),
        "x={setting:obj}"
    );
    assert_eq!(
        interpolate_hint_settings("id={setting:bot_uin}", None),
        "id={setting:bot_uin}"
    );
    assert_eq!(
        interpolate_hint_settings("id={setting:bot_uin", Some(&json!({ "bot_uin": "1" }))),
        "id={setting:bot_uin"
    );
}

#[test]
fn text_without_placeholders_is_unchanged() {
    let hint = "plain hint with braces {not-a-setting} and {}";
    assert_eq!(interpolate_hint_settings(hint, Some(&json!({}))), hint);
}
