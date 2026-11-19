//! Doc-contract check for the checked-in agent interface file.
//!
//! The runtime rewrites `docs/agent/interfaces.json` from the dylib's exported
//! `docs_value()` during an artifact build, so this test pins the properties
//! the agent relies on: one agent-accessible `generate_image` node whose only
//! required argument is the prompt.

use serde_json::Value;

#[test]
fn docs_declare_an_agent_accessible_generate_image_node() {
    let text = include_str!("../docs/agent/interfaces.json");
    let docs: Value = serde_json::from_str(text).expect("interfaces.json is valid JSON");

    assert_eq!(docs["plugin_path"], "image_gen");
    assert_eq!(docs["plugin_version"], "0.1.0");

    let nodes = docs["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 1, "image_gen declares exactly one node");
    let node = &nodes[0];
    assert_eq!(node["id"], "generate_image");
    assert_eq!(node["agent_accessible"], true);
    assert_eq!(node["node_type"], "router");

    let required = node["input_schema"]["required"]
        .as_array()
        .expect("required list");
    assert!(required.contains(&Value::from("prompt")));
    assert!(required.contains(&Value::from("node_id")));

    let hint = docs["system_hint"].as_str().expect("system_hint");
    assert!(
        hint.contains("cq_image"),
        "the hint must say how to send the file"
    );
    assert!(
        hint.contains("view_image") && hint.contains("sha256"),
        "the hint must say how the agent re-reads its own picture"
    );

    let outputs = &node["output_schema"]["properties"];
    assert!(
        outputs.get("sha256").is_some(),
        "the result must carry the digest"
    );
    assert!(outputs.get("media_path").is_some());
    assert!(outputs.get("media_cached").is_some());
    assert!(
        node["side_effects"]
            .as_array()
            .expect("side effects")
            .iter()
            .any(|effect| effect
                .as_str()
                .is_some_and(|text| text.contains("data/media/"))),
        "the cache write must be declared as a side effect"
    );
}
