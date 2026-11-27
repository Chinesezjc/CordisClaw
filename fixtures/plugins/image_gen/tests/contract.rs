//! Doc-contract check for the checked-in agent interface file.
//!
//! The runtime rewrites `docs/agent/interfaces.json` from the dylib's exported
//! `docs_value()` during an artifact build, so this test pins the properties
//! the agent relies on: agent-accessible `generate_image` and `edit_image`
//! nodes that return the same result shape.

use serde_json::Value;

#[test]
fn docs_declare_both_agent_accessible_image_nodes() {
    let text = include_str!("../docs/agent/interfaces.json");
    let docs: Value = serde_json::from_str(text).expect("interfaces.json is valid JSON");

    assert_eq!(docs["plugin_path"], "image_gen");
    assert_eq!(docs["plugin_version"], "0.1.0");

    let nodes = docs["nodes"].as_array().expect("nodes array");
    assert_eq!(
        nodes.len(),
        2,
        "image_gen declares generate_image and edit_image"
    );
    let generate = nodes
        .iter()
        .find(|node| node["id"] == "generate_image")
        .expect("generate_image node");
    let edit = nodes
        .iter()
        .find(|node| node["id"] == "edit_image")
        .expect("edit_image node");

    for node in [generate, edit] {
        assert_eq!(node["agent_accessible"], true);
        assert_eq!(node["node_type"], "router");

        let outputs = &node["output_schema"]["properties"];
        for field in ["path", "cq_image", "sha256", "media_path", "bytes"] {
            assert!(
                outputs.get(field).is_some(),
                "{} must return {field}",
                node["id"]
            );
        }
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

    let required = generate["input_schema"]["required"]
        .as_array()
        .expect("required list");
    assert!(required.contains(&Value::from("prompt")));
    assert!(required.contains(&Value::from("node_id")));

    let edit_required = edit["input_schema"]["required"]
        .as_array()
        .expect("edit required list");
    for field in ["node_id", "image", "prompt"] {
        assert!(
            edit_required.contains(&Value::from(field)),
            "edit_image must require {field}"
        );
    }
    assert!(
        edit["input_schema"]["properties"].get("image").is_some(),
        "edit_image must document its base image argument"
    );

    let hint = docs["system_hint"].as_str().expect("system_hint");
    assert!(
        hint.contains("cq_image"),
        "the hint must say how to send the file"
    );
    assert!(
        hint.contains("view_image") && hint.contains("sha256"),
        "the hint must say how the agent re-reads its own picture"
    );
    assert!(
        hint.contains("edit_image"),
        "the hint must mention the edit node"
    );
    assert!(
        hint.contains("outside"),
        "the hint must warn that non-cached base images are refused"
    );
}
