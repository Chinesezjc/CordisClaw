//! 把 node 文档压成「一行一个节点」的目录，喂进 system prompt。
//!
//! 为什么需要它：`docs.nodes[].summary / input_schema / output_schema` 一直存在
//! 于工件里，但 `agent_plugin_hints` 只列了 node id —— 模型知道有个节点叫
//! `qq_get_group_info`，却不知道它返回 `member_count`，于是插件不得不在
//! `system_hint` 里用手写英文把同样的事再讲一遍（还会和实现漂移）。
//!
//! 这里只做压缩渲染，事实仍然来自工件文档：摘要截断、只列一层字段、
//! 对象字段展开成 `data{a,b}`（最多两层），因此目录里的内容不会和实现脱节。

use cordis_plugin_sdk::NodeDoc;
use serde_json::Value;

/// 摘要最多保留多少字符（再多就截断加省略号）。
pub const MAX_SUMMARY_CHARS: usize = 160;
/// 每一层最多列几个字段名，防止某个节点的 schema 把目录撑爆。
const MAX_KEYS: usize = 8;
/// 最多展开几层嵌套对象（`data{...data{...}}` 就是两层）。
const MAX_DEPTH: usize = 2;

/// 渲染单个节点：`id — summary (in: a,b) → out: ok,data{x,y}`。
///
/// 三段都可缺：没有摘要就只有 id/入参/出参，没有 schema 就没有对应那一段。
pub fn describe_node(node: &NodeDoc) -> String {
    let mut line = node.id.clone();
    let summary = truncate(node.summary.trim(), MAX_SUMMARY_CHARS);
    if !summary.is_empty() {
        line.push_str(" — ");
        line.push_str(&summary);
    }
    let inputs = required_fields(&node.input_schema);
    if !inputs.is_empty() {
        line.push_str(&format!(" (in: {})", inputs.join(",")));
    }
    let outputs = render_fields(&node.output_schema, 0);
    if !outputs.is_empty() {
        line.push_str(&format!(" → out: {outputs}"));
    }
    line
}

/// 目录头。调用方可以先 `String::from(CATALOG_HEADER)` 再逐插件追加。
pub const CATALOG_HEADER: &str = "Loaded plugins (this catalog is generated from the loaded \
artifacts' node docs — it is the source of truth for what each node takes and returns):\n";

/// 追加一个插件及其节点（`nodes` 只应包含 agent 可见的节点）。
pub fn push_plugin(catalog: &mut String, plugin_path: &str, nodes: &[&NodeDoc]) {
    if nodes.is_empty() {
        return;
    }
    catalog.push_str(&format!("  {plugin_path} →\n"));
    for node in nodes {
        catalog.push_str(&format!("    {}\n", describe_node(node)));
    }
}

/// 渲染整份目录（`push_plugin` 的批量版本，主要给测试用）。
pub fn render_catalog(plugins: &[(&str, Vec<&NodeDoc>)]) -> String {
    let mut catalog = String::from(CATALOG_HEADER);
    for (plugin_path, nodes) in plugins {
        push_plugin(&mut catalog, plugin_path, nodes);
    }
    catalog
}

fn truncate(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// `input_schema.required`（拿不到就退回 `properties` 的键）。
fn required_fields(schema: &Value) -> Vec<String> {
    let required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .take(MAX_KEYS)
                .collect()
        })
        .unwrap_or_default();
    if !required.is_empty() {
        return required;
    }
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| props.keys().take(MAX_KEYS).cloned().collect())
        .unwrap_or_default()
}

/// 把 `properties` 渲染成 `a,b,nested{x,y}`，最多展开 [`MAX_DEPTH`] 层。
fn render_fields(schema: &Value, depth: usize) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in props.iter().take(MAX_KEYS) {
        let nested = value.get("properties").and_then(Value::as_object);
        let expandable = nested.is_some() && depth < MAX_DEPTH;
        if expandable {
            let inner = render_fields(value, depth + 1);
            parts.push(format!("{key}{{{inner}}}"));
        } else {
            parts.push(key.clone());
        }
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, summary: &str, input: Value, output: Value) -> NodeDoc {
        NodeDoc {
            id: id.to_string(),
            summary: summary.to_string(),
            input_schema: input,
            output_schema: output,
            side_effects: vec![],
            failure_modes: vec![],
            node_type: cordis_plugin_sdk::NodeType::Router,
            agent_accessible: true,
        }
    }

    #[test]
    fn describes_id_summary_inputs_and_outputs() {
        let doc = node(
            "qq_get_group_info",
            "Get group information (name, member count, etc.).",
            json!({"type": "object", "required": ["node_id", "target"]}),
            json!({
                "type": "object",
                "properties": {
                    "ok": {"type": "boolean"},
                    "message": {"type": ["string", "null"]},
                    "data": {
                        "type": "object",
                        "properties": {
                            "status": {"type": "string"},
                            "data": {
                                "type": "object",
                                "properties": {
                                    "group_name": {"type": "string"},
                                    "member_count": {"type": "integer"}
                                }
                            }
                        }
                    }
                }
            }),
        );
        let line = describe_node(&doc);
        let starts_right = line.starts_with("qq_get_group_info — Get group information");
        assert!(starts_right, "{line}");
        let has_inputs = line.contains("(in: node_id,target)");
        assert!(has_inputs, "{line}");
        let has_outputs =
            line.contains("out: ok,message,data{status,data{group_name,member_count}}");
        assert!(has_outputs, "{line}");
    }

    #[test]
    fn truncates_a_long_summary() {
        let long = "x".repeat(MAX_SUMMARY_CHARS + 20);
        let doc = node("n", &long, json!({}), json!({}));
        let line = describe_node(&doc);
        let body = line.trim_start_matches("n — ");
        let chars = body.chars().count();
        assert_eq!(chars, MAX_SUMMARY_CHARS + 1, "summary + ellipsis: {body}");
        let ends_with_ellipsis = body.ends_with('…');
        assert!(ends_with_ellipsis, "{body}");
    }

    #[test]
    fn empty_summary_and_missing_schemas_degrade_to_id_only() {
        let doc = node("bare", "   ", json!({}), json!({}));
        assert_eq!(describe_node(&doc), "bare");
    }

    #[test]
    fn required_falls_back_to_property_names() {
        let doc = node(
            "n",
            "s",
            json!({"type": "object", "properties": {"a": {}, "b": {}}}),
            json!({}),
        );
        let line = describe_node(&doc);
        let has_fallback_inputs = line.contains("(in: a,b)");
        assert!(has_fallback_inputs, "{line}");
    }

    #[test]
    fn catalog_lists_plugins_and_their_nodes() {
        let a = node("alpha", "first", json!({}), json!({}));
        let b = node("beta", "second", json!({}), json!({}));
        let catalog = render_catalog(&[("qq", vec![&a, &b])]);
        let has_plugin = catalog.contains("  qq →");
        assert!(has_plugin, "{catalog}");
        let has_alpha = catalog.contains("    alpha — first");
        assert!(has_alpha, "{catalog}");
        let has_beta = catalog.contains("    beta — second");
        assert!(has_beta, "{catalog}");
        let declares_truth = catalog.contains("source of truth");
        assert!(declares_truth, "{catalog}");
    }

    #[test]
    fn empty_catalog_still_has_the_header() {
        let catalog = render_catalog(&[]);
        let has_header = catalog.starts_with("Loaded plugins");
        assert!(has_header, "{catalog}");
    }

    #[test]
    fn caps_the_number_of_keys_per_level() {
        let mut props = serde_json::Map::new();
        for i in 0..(MAX_KEYS + 4) {
            props.insert(format!("k{i}"), json!({"type": "string"}));
        }
        let doc = node(
            "n",
            "s",
            json!({"type": "object", "properties": props}),
            json!({"type": "object", "properties": props}),
        );
        let line = describe_node(&doc);
        let out_part = line.split("→ out: ").nth(1).unwrap_or_default();
        assert_eq!(out_part.split(',').count(), MAX_KEYS, "{line}");
    }
}
