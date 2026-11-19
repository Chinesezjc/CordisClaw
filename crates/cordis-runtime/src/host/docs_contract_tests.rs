use super::docs_contract_changed;
use cordis_plugin_sdk::{NodeDoc, NodeType, PluginDocs};

fn docs(nodes: Vec<(&str, NodeType, &str)>) -> PluginDocs {
    PluginDocs {
        plugin_id: "qq".to_string(),
        plugin_path: "qq".to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: None,
        nodes: nodes
            .into_iter()
            .map(|(id, node_type, summary)| NodeDoc {
                id: id.to_string(),
                summary: summary.to_string(),
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                side_effects: Vec::new(),
                failure_modes: Vec::new(),
                node_type,
                agent_accessible: true,
            })
            .collect(),
        system_hint: None,
    }
}

// Rewording docs after a rebuild is legitimate and must be adopted; only a
// changed node set is a contract change.
#[test]
fn only_a_changed_node_set_is_a_contract_change() {
    let before = docs(vec![
        ("qq_send", NodeType::Task, "send a message"),
        ("qq_serve", NodeType::Task, "http server"),
    ]);
    let reworded = docs(vec![
        (
            "qq_send",
            NodeType::Task,
            "send a message (CQ codes pass through)",
        ),
        ("qq_serve", NodeType::Task, "http server"),
    ]);
    assert!(!docs_contract_changed(&before, &before));
    assert!(!docs_contract_changed(&before, &reworded));
    // Order must not matter.
    let reordered = docs(vec![
        ("qq_serve", NodeType::Task, "http server"),
        ("qq_send", NodeType::Task, "send a message"),
    ]);
    assert!(!docs_contract_changed(&before, &reordered));

    // A new node, a removed node or a changed node type are contract changes.
    let mut added = reworded.clone();
    added.nodes.push(NodeDoc {
        id: "qq_extra".to_string(),
        summary: "new".to_string(),
        input_schema: serde_json::json!({}),
        output_schema: serde_json::json!({}),
        side_effects: Vec::new(),
        failure_modes: Vec::new(),
        node_type: NodeType::Task,
        agent_accessible: true,
    });
    assert!(docs_contract_changed(&before, &added));
    let mut removed = reworded.clone();
    removed.nodes.retain(|node| node.id == "qq_send");
    assert!(docs_contract_changed(&before, &removed));
    let mut retyped = reworded.clone();
    retyped.nodes[0].node_type = NodeType::Router;
    assert!(docs_contract_changed(&before, &retyped));
}
