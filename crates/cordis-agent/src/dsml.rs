//! The DSML tool-call dialect the model actually writes.
//!
//! We used to ask for JSON wrapped in `<|DSML|tool_calls>` tags. The model
//! rarely produced that: across 685 stored sessions it emitted an XML-ish
//! dialect 93 times —
//!
//! ```text
//! <|DSML|calls>
//! <|DSML|invoke name="search_code">
//! <|DSML|parameter name="path" string="true">plugins</|DSML|parameter>
//! <|DSML|parameter name="pattern" string="true">欢迎</|DSML|parameter>
//! </|DSML|invoke>
//! </|DSML|calls>
//! ```
//!
//! — which the JSON scanner could not read, so those turns produced no call at
//! all. Rather than keep fighting the model's preference, this module makes the
//! dialect official: it is what we render for the prompt, what history shows,
//! and what the parser accepts. The JSON form stays as a fallback so old
//! snapshots still replay.
//!
//! The pipes are accepted in both widths: the model writes the fullwidth `｜`
//! (U+FF5C) about as often as the ASCII `|`.

use serde_json::{Map, Value};

/// One parsed `<|DSML|invoke>` block.
#[derive(Debug, Clone, PartialEq)]
pub struct DsmlCall {
    /// Tool or plugin-node name.
    pub name: String,
    /// Parameters in source order; values are raw text until coercion.
    pub params: Vec<(String, String)>,
    /// Whether the parameter was explicitly marked `string="true"`.
    pub string_flags: Vec<bool>,
}

impl DsmlCall {
    /// Parameters as a JSON object, coercing the values the model wrote.
    ///
    /// Text stays text unless the model either marked it `string="true"` or
    /// wrote something that is unmistakably JSON (an object, an array, a
    /// number, `true`/`false`/`null`). Chat text such as `123` therefore stays
    /// a string only when the flag is present — which is exactly why the
    /// rendered form always emits the flag.
    pub fn arguments(&self) -> Value {
        let mut object = Map::new();
        for ((name, raw), is_string) in self.params.iter().zip(&self.string_flags) {
            // `payload_json` is nested JSON by definition; the model marks it
            // `string="true"` anyway, and passing the object through as text
            // makes every plugin see a missing field.
            let is_string = *is_string && name != "payload_json";
            let value = if is_string {
                Value::String(raw.clone())
            } else {
                coerce(raw)
            };
            object.insert(name.clone(), value);
        }
        Value::Object(object)
    }
}

/// Raw text to JSON when it is clearly a literal, otherwise a string.
fn coerce(raw: &str) -> Value {
    let trimmed = raw.trim();
    let looks_structured = trimmed.starts_with('{')
        || trimmed.starts_with('[')
        || trimmed == "true"
        || trimmed == "false"
        || trimmed == "null"
        || is_number(trimmed);
    if looks_structured {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            return value;
        }
    }
    Value::String(raw.to_string())
}

fn is_number(text: &str) -> bool {
    let chars = text.chars();
    let Some(first) = chars.clone().next() else {
        return false;
    };
    if !(first.is_ascii_digit() || first == '-') {
        return false;
    }
    text.parse::<f64>().is_ok()
}

/// Fold the spellings the model mixes into one.
///
/// Two things vary in practice: the pipe width (`|` vs the fullwidth `\u{ff5c}`),
/// and a doubled pipe — the model often writes the provider's native
/// `<||DSML||calls>` even though we ask for `<|DSML|calls>`. The doubling is
/// folded only inside the tag, so ordinary text such as `a || b` survives.
fn normalize(text: &str) -> String {
    let narrowed = text
        .replace('\u{ff5c}', "|")
        .replace("||DSML||", "|DSML|")
        .replace("||dsml||", "|dsml|");
    // The model also spaces the tag name out: `<|DSML| invoke name=...>`.
    let mut out = String::with_capacity(narrowed.len());
    let bytes = narrowed.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if narrowed[index..].to_ascii_lowercase().starts_with("|dsml|") {
            out.push_str(&narrowed[index..index + 6]);
            index += 6;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            continue;
        }
        let ch = narrowed[index..].chars().next().expect("char");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// Does this text carry DSML markup at all?
pub fn looks_like_dsml(content: &str) -> bool {
    let normalized = normalize(content).to_ascii_lowercase();
    normalized.contains("<|dsml|")
}

/// Every `<|DSML|invoke>` block in `text`, in order.
///
/// Missing closing tags are tolerated: an unterminated invoke runs to the next
/// opening tag or to the end, because a truncated stream is far more common
/// than a nested block.
pub fn parse(text: &str) -> Vec<DsmlCall> {
    let normalized = normalize(text);
    let lower = normalized.to_ascii_lowercase();
    let mut calls = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = lower[cursor..].find("<|dsml|invoke") {
        let start = cursor + found;
        let Some(open_end) = normalized[start..].find('>').map(|offset| start + offset) else {
            break;
        };
        let open_tag = &normalized[start..=open_end];
        let name = attribute(open_tag, "name").unwrap_or_default();
        if name.is_empty() {
            cursor = open_end + 1;
            continue;
        }
        let body_start = open_end + 1;
        // The block ends at its own closing tag, or at the next invoke.
        let close = lower[body_start..]
            .find("</|dsml|invoke")
            .map(|offset| body_start + offset);
        let next = lower[body_start..]
            .find("<|dsml|invoke")
            .map(|offset| body_start + offset);
        let body_end = match (close, next) {
            (Some(close), Some(next)) => close.min(next),
            (Some(close), None) => close,
            (None, Some(next)) => next,
            (None, None) => normalized.len(),
        };
        let body = &normalized[body_start..body_end];
        let (params, string_flags) = parse_params(body);
        calls.push(DsmlCall {
            name,
            params,
            string_flags,
        });
        cursor = close.map(|close| close + 1).unwrap_or(body_end);
        if cursor >= normalized.len() {
            break;
        }
    }
    calls
}

fn parse_params(body: &str) -> (Vec<(String, String)>, Vec<bool>) {
    let lower = body.to_ascii_lowercase();
    let mut params = Vec::new();
    let mut flags = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = lower[cursor..].find("<|dsml|") {
        let start = cursor + found;
        let after = start + "<|dsml|".len();
        let Some(open_end) = body[after..].find('>').map(|offset| after + offset) else {
            break;
        };
        let open_tag = &body[start..=open_end];
        // The model sometimes writes the parameter name as the tag itself —
        // `<|DSML|node_id" string="true">qq_send</|DSML|parameter>` — instead of
        // `<|DSML|parameter name="node_id" …>`. Either spelling means the same.
        let word: String = body[after..open_end]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let (name, is_string) = if word.eq_ignore_ascii_case("parameter") {
            (
                attribute(open_tag, "name").unwrap_or_default(),
                attribute(open_tag, "string")
                    .map(|value| value.eq_ignore_ascii_case("true"))
                    .unwrap_or(false),
            )
        } else if word.is_empty()
            || word.eq_ignore_ascii_case("invoke")
            || word.eq_ignore_ascii_case("calls")
            || word.eq_ignore_ascii_case("tool_calls")
        {
            cursor = open_end + 1;
            continue;
        } else {
            (
                word,
                attribute(open_tag, "string")
                    .map(|value| value.eq_ignore_ascii_case("true"))
                    .unwrap_or(false),
            )
        };
        let value_start = open_end + 1;
        let close = lower[value_start..]
            .find("</|dsml|parameter")
            .map(|offset| value_start + offset)
            .unwrap_or(body.len());
        let value = body[value_start..close].to_string();
        if !name.is_empty() {
            params.push((name, value));
            flags.push(is_string);
        }
        cursor = if close < body.len() {
            close + 1
        } else {
            body.len()
        };
    }
    (params, flags)
}

/// Read `key="value"` out of an opening tag.
fn attribute(tag: &str, key: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{key}=");
    let mut from = 0usize;
    while let Some(found) = lower[from..].find(&needle) {
        let at = from + found;
        // Only accept the attribute when it starts a word, so `name=` does not
        // match inside `filename=`.
        let boundary = tag[..at]
            .chars()
            .last()
            .map(|c| c.is_whitespace() || c == '<' || c == '|')
            .unwrap_or(false);
        if !boundary {
            from = at + needle.len();
            continue;
        }
        let rest = tag[at + needle.len()..].trim_start();
        let mut chars = rest.chars();
        let quote = chars.next()?;
        if quote != '"' && quote != '\'' {
            from = at + needle.len();
            continue;
        }
        let value: String = chars.take_while(|c| *c != quote).collect();
        return Some(value);
    }
    None
}

/// Render calls in the canonical dialect — used for the prompt and history.
pub fn render(calls: &[(String, Value)]) -> String {
    let mut text = String::from("<|DSML|calls>\n");
    for (name, arguments) in calls {
        text.push_str(&format!("<|DSML|invoke name=\"{name}\">\n"));
        if let Some(object) = arguments.as_object() {
            for (key, value) in object {
                let (raw, flag) = match value {
                    Value::String(text) => (text.clone(), " string=\"true\""),
                    other => (other.to_string(), ""),
                };
                text.push_str(&format!(
                    "<|DSML|parameter name=\"{key}\"{flag}>{raw}</|DSML|parameter>\n"
                ));
            }
        }
        text.push_str("</|DSML|invoke>\n");
    }
    text.push_str("</|DSML|calls>");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_dialect_the_model_actually_emitted() {
        let text = "<｜｜DSML｜｜ calls>\n\
                    <｜｜DSML｜｜ invoke name=\"search_code\">\n\
                    <｜｜DSML｜｜ parameter name=\"path\" string=\"true\">plugins</｜｜DSML｜｜ parameter>\n\
                    <｜｜DSML｜｜ parameter name=\"pattern\" string=\"true\">欢迎</｜｜DSML｜｜ parameter>\n\
                    </｜｜DSML｜｜ invoke>\n\
                    </｜｜DSML｜｜ calls>";
        let calls = parse(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "search_code");
        assert_eq!(
            calls[0].arguments(),
            serde_json::json!({"path": "plugins", "pattern": "欢迎"})
        );
    }

    #[test]
    fn parses_several_invokes_and_an_empty_one() {
        let text = "<|DSML|calls>\n\
                    <|DSML|invoke name=\"suspend\">\n\n</|DSML|invoke>\n\
                    <|DSML|invoke name=\"qq_send\">\n\
                    <|DSML|parameter name=\"target\" string=\"true\">group:1</|DSML|parameter>\n\
                    <|DSML|parameter name=\"message\" string=\"true\">hi</|DSML|parameter>\n\
                    </|DSML|invoke>\n\
                    </|DSML|calls>";
        let calls = parse(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "suspend");
        assert!(calls[0].params.is_empty());
        assert_eq!(calls[1].name, "qq_send");
        assert_eq!(
            calls[1].arguments(),
            serde_json::json!({"target": "group:1", "message": "hi"})
        );
    }

    #[test]
    fn json_parameters_are_coerced_and_text_is_not() {
        let text = "<|DSML|invoke name=\"invoke_plugin\">\
                    <|DSML|parameter name=\"plugin_path\" string=\"true\">qq</|DSML|parameter>\
                    <|DSML|parameter name=\"node_id\" string=\"true\">qq_send</|DSML|parameter>\
                    <|DSML|parameter name=\"payload_json\">{\"target\":\"group:1\"}</|DSML|parameter>\
                    </|DSML|invoke>";
        let calls = parse(text);
        assert_eq!(
            calls[0].arguments(),
            serde_json::json!({
                "plugin_path": "qq",
                "node_id": "qq_send",
                "payload_json": {"target": "group:1"}
            })
        );
    }

    #[test]
    fn a_number_stays_text_when_the_model_says_string() {
        let call = DsmlCall {
            name: "qq_send".to_string(),
            params: vec![("message".to_string(), "123".to_string())],
            string_flags: vec![true],
        };
        assert_eq!(call.arguments(), serde_json::json!({"message": "123"}));
        let call = DsmlCall {
            string_flags: vec![false],
            ..call
        };
        assert_eq!(call.arguments(), serde_json::json!({"message": 123}));
    }

    #[test]
    fn an_unterminated_block_still_yields_the_call() {
        let calls = parse("<|DSML|invoke name=\"suspend\">");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "suspend");
    }

    #[test]
    fn a_body_without_parameters_is_an_empty_argument_object() {
        let calls = parse("<|DSML|invoke name=\"suspend\">\n\n</|DSML|invoke>");
        assert_eq!(calls[0].arguments(), serde_json::json!({}));
    }

    #[test]
    fn an_invoke_without_a_name_is_skipped() {
        let calls =
            parse("<|DSML|invoke><|DSML|parameter name=\"a\">b</|DSML|parameter></|DSML|invoke>");
        assert!(calls.is_empty());
    }

    /// The model slips and writes the parameter name as the tag:
    /// `<|DSML|node_id" string="true">qq_send</|DSML|parameter>`. Dropping that
    /// parameter made `invoke_plugin` arrive without a node and fail.
    #[test]
    fn a_parameter_written_as_its_own_tag_still_parses() {
        let text = "<|DSML|invoke name=\"invoke_plugin\">\
                    <|DSML|plugin_path\" string=\"true\">qq</|DSML|parameter>\
                    <|DSML|node_id\" string=\"true\">qq_send</|DSML|parameter>\
                    </|DSML|invoke>";
        let calls = parse(text);
        assert_eq!(
            calls[0].arguments(),
            serde_json::json!({"plugin_path": "qq", "node_id": "qq_send"})
        );
    }

    /// `payload_json` is nested JSON even when the model flags it as a string.
    #[test]
    fn payload_json_is_json_despite_the_string_flag() {
        let text = "<|DSML|invoke name=\"invoke_plugin\">\
                    <|DSML|parameter name=\"plugin_path\" string=\"true\">qq</|DSML|parameter>\
                    <|DSML|parameter name=\"node_id\" string=\"true\">qq_send</|DSML|parameter>\
                    <|DSML|parameter name=\"payload_json\" string=\"true\">{\"target\": \"group:1\"}</|DSML|parameter>\
                    </|DSML|invoke>";
        let calls = parse(text);
        assert_eq!(
            calls[0].arguments()["payload_json"],
            serde_json::json!({"target": "group:1"})
        );
    }

    #[test]
    fn plain_text_is_not_dsml() {
        assert!(!looks_like_dsml("just a reply about <tool calls>"));
        assert!(looks_like_dsml("<|DSML|calls>"));
        assert!(looks_like_dsml("<｜｜DSML｜｜ calls>"));
    }

    #[test]
    fn rendering_round_trips_through_the_parser() {
        let calls = vec![
            ("suspend".to_string(), serde_json::json!({})),
            (
                "qq_send".to_string(),
                serde_json::json!({"target": "group:1", "message": "hi"}),
            ),
        ];
        let text = render(&calls);
        let parsed = parse(&text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "suspend");
        assert_eq!(parsed[1].arguments(), calls[1].1);
    }

    #[test]
    fn rendering_marks_text_parameters_as_strings() {
        let text = render(&[(
            "qq_send".to_string(),
            serde_json::json!({"message": "hi", "n": 3}),
        )]);
        assert!(text.contains("name=\"message\" string=\"true\""), "{text}");
        // Numbers are rendered as JSON, so they must not claim to be strings.
        assert!(text.contains("name=\"n\">3"), "{text}");
    }

    #[test]
    fn attributes_do_not_match_inside_longer_names() {
        let calls = parse(
            "<|DSML|invoke xname=\"no\" name=\"yes\">\
             <|DSML|parameter xname=\"no\" name=\"a\" string=\"true\">b</|DSML|parameter>\
             </|DSML|invoke>",
        );
        assert_eq!(calls[0].name, "yes");
        assert_eq!(calls[0].params, vec![("a".to_string(), "b".to_string())]);
    }

    #[test]
    fn single_quoted_attributes_parse_too() {
        let calls = parse("<|DSML|invoke name='read_file'><|DSML|parameter name='path' string='true'>/tmp/a</|DSML|parameter></|DSML|invoke>");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments(), serde_json::json!({"path": "/tmp/a"}));
    }

    /// A parameter the model did not mark as a string still stays text when it
    /// is not a JSON literal — chat text must not be reinterpreted.
    #[test]
    fn unmarked_plain_text_parameters_stay_text() {
        let calls = parse(
            "<|DSML|invoke name=\"qq_send\">\
             <|DSML|parameter name=\"message\">hello</|DSML|parameter>\
             </|DSML|invoke>",
        );
        assert_eq!(
            calls[0].arguments(),
            serde_json::json!({"message": "hello"})
        );
    }

    /// Something that opens like a JSON object but does not parse falls back to
    /// the raw text instead of being dropped.
    #[test]
    fn a_structured_looking_value_that_is_not_json_stays_text() {
        let calls = parse(
            "<|DSML|invoke name=\"qq_send\">\
             <|DSML|parameter name=\"message\">{oops</|DSML|parameter>\
             </|DSML|invoke>",
        );
        assert_eq!(
            calls[0].arguments(),
            serde_json::json!({"message": "{oops"})
        );
    }

    /// An empty value is not a number and not JSON, so it stays an empty string.
    #[test]
    fn an_empty_parameter_value_stays_text() {
        let calls = parse(
            "<|DSML|invoke name=\"qq_send\">\
             <|DSML|parameter name=\"message\"></|DSML|parameter>\
             </|DSML|invoke>",
        );
        assert_eq!(calls[0].arguments(), serde_json::json!({"message": ""}));
    }

    /// A truncated `<|DSML|invoke` with no closing `>` cannot name a tool, so
    /// the scan stops rather than inventing one.
    #[test]
    fn an_open_tag_without_a_closing_angle_bracket_is_ignored() {
        assert!(parse("<|DSML|invoke name=\"read_file\"").is_empty());
    }

    /// An unterminated invoke body runs to the next invoke, not to the end of
    /// the text: the second call must still be seen.
    #[test]
    fn a_truncated_invoke_stops_at_the_next_invoke() {
        let calls = parse(
            "<|DSML|invoke name=\"first\">one\
             <|DSML|invoke name=\"second\">two",
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "first");
        assert_eq!(calls[1].name, "second");
        assert!(calls[0].params.is_empty());
    }

    /// A `<|DSML|calls>` wrapper nested inside an invoke body is a container
    /// tag, not a parameter, so it is skipped instead of becoming a parameter
    /// named `calls`.
    #[test]
    fn a_nested_calls_tag_is_not_a_parameter() {
        let calls = parse("<|DSML|invoke name=\"suspend\"><|DSML|calls></|DSML|invoke>");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "suspend");
        assert!(calls[0].params.is_empty());
    }

    /// A parameter whose closing tag is missing keeps the rest of the body as
    /// its value, because a truncated stream is the common case.
    #[test]
    fn an_unterminated_parameter_keeps_the_rest_of_the_body() {
        let calls = parse("<|DSML|invoke name=\"read_file\"><|DSML|parameter name=\"path\">/tmp/a");
        assert_eq!(calls[0].arguments(), serde_json::json!({"path": "/tmp/a"}));
    }

    /// A parameter tag cut off before its own `>` cannot be read at all, so the
    /// body scan stops instead of treating the fragment as a parameter.
    #[test]
    fn a_parameter_tag_without_a_closing_angle_bracket_stops_the_scan() {
        let calls = parse("<|DSML|invoke name=\"suspend\">text <|DSML|parameter");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "suspend");
        assert!(calls[0].params.is_empty());
    }

    /// An attribute whose value is not quoted cannot be read, so the invoke has
    /// no name and is skipped rather than guessed at.
    #[test]
    fn an_unquoted_attribute_value_is_not_read() {
        assert!(parse("<|DSML|invoke name=read_file>").is_empty());
    }

    /// Rendering is defined for any JSON value, including one that is not an
    /// object: such a call renders with no parameters at all.
    #[test]
    fn rendering_a_non_object_argument_emits_no_parameters() {
        let text = render(&[("weird".to_string(), serde_json::json!([1, 2]))]);
        assert!(text.contains("<|DSML|invoke name=\"weird\">"), "{text}");
        assert!(!text.contains("<|DSML|parameter"), "{text}");
    }
}
