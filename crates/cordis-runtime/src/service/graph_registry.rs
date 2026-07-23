use crate::core::error::RuntimeError;
use crate::core::models::{NodeDoc, PluginLoadResult};
use crate::plugin::registry::{NodeRegistry, PluginRegistry};
use crate::service::html_render::HtmlWriter;
use cordis_plugin_sdk::NodeType;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Default, Clone, Serialize)]
pub struct RegisteredGraph {
    pub plugins: Vec<RegisteredPluginGraph>,
    pub nodes: Vec<RegisteredNodeGraph>,
    pub edges: Vec<RegisteredGraphEdge>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredPluginGraph {
    pub plugin_path: String,
    pub parent: Option<String>,
    pub required: bool,
    pub load_result: PluginLoadResult,
    pub node_count: usize,
    pub depth: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredNodeGraph {
    pub node_fqn: String,
    pub plugin_path: String,
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: RegisteredGraphEdgeKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredGraphEdgeKind {
    PluginChild,
    PluginNode,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RegisteredNet {
    pub nodes: Vec<RegisteredNetNode>,
    pub edges: Vec<RegisteredNetEdge>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredNetNode {
    pub node_fqn: String,
    pub plugin_path: String,
    pub node_id: String,
    pub consumes: Vec<String>,
    pub produces: Vec<String>,
    pub topo_level: usize,
    pub node_type: NodeType,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredNetEdge {
    pub from: String,
    pub to: String,
    pub kind: RegisteredNetEdgeKind,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredNetEdgeKind {
    Data,
    /// Reserved: pure ordering edge (B runs after A, no data dependency).
    /// Not yet inferred from schemas — requires explicit dependency
    /// declarations in [`NodeDoc`] or a separate control-flow inference pass.
    Control,
}

#[derive(Debug, Default, Clone)]
pub struct GraphRegistry {
    registration_graph: RegisteredGraph,
    net_graph: RegisteredNet,
}

impl GraphRegistry {
    pub fn from_registries(plugin_registry: &PluginRegistry, node_registry: &NodeRegistry) -> Self {
        let registration_graph = build_registration_graph(plugin_registry, node_registry);
        let net_graph = build_registered_net(plugin_registry, node_registry);
        Self {
            registration_graph,
            net_graph,
        }
    }

    pub fn graph(&self) -> &RegisteredGraph {
        &self.registration_graph
    }

    pub fn net(&self) -> &RegisteredNet {
        &self.net_graph
    }

    pub fn handle_get_json(&self, path: &str) -> Result<Value, RuntimeError> {
        match path {
            "/graphs/registered-nodes" => {
                serde_json::to_value(&self.registration_graph).map_err(|err| {
                    RuntimeError::Invariant {
                        message: format!("serialize registered graph failed: {err}"),
                    }
                })
            }
            "/graphs/registered-net" => {
                serde_json::to_value(&self.net_graph).map_err(|err| RuntimeError::Invariant {
                    message: format!("serialize registered net failed: {err}"),
                })
            }
            _ => Err(RuntimeError::InvalidDocsRoute {
                path: path.to_string(),
            }),
        }
    }

    pub fn handle_get_html(&self, path: &str) -> Result<String, RuntimeError> {
        match path {
            "/graphs/registered-nodes.html" => Ok(self.render_registered_nodes_html()),
            "/graphs/registered-net.html" => Ok(self.render_registered_net_html()),
            _ => Err(RuntimeError::InvalidDocsRoute {
                path: path.to_string(),
            }),
        }
    }

    pub fn render_registered_nodes_html(&self) -> String {
        let mut w = HtmlWriter::new();
        w.raw("<!doctype html><html><head><meta charset=\"utf-8\"><title>Registered Nodes Graph</title></head><body>");
        w.raw("<h1>Registered Nodes Graph</h1>");

        w.raw("<h2>Plugins</h2><ul>");
        for plugin in &self.registration_graph.plugins {
            w.open_tag("li");
            w.text(&plugin.plugin_path);
            w.raw(" (");
            w.text(&format!("{:?}", plugin.load_result));
            w.raw(")");
            w.close_tag("li");
        }
        w.raw("</ul>");

        w.raw("<h2>Nodes</h2><ul>");
        for node in &self.registration_graph.nodes {
            w.open_tag("li");
            w.text(&node.node_fqn);
            w.raw(" :: ");
            w.text(&node.node_id);
            w.close_tag("li");
        }
        w.raw("</ul>");

        w.raw("</body></html>");
        w.into_string()
    }

    pub fn render_registered_net_html(&self) -> String {
        let mut w = HtmlWriter::new();
        w.raw("<!doctype html><html><head><meta charset=\"utf-8\"><title>Registered Net</title></head><body>");
        w.raw("<h1>Registered Net</h1>");

        if !self.net_graph.diagnostics.is_empty() {
            w.raw("<h2>Net diagnostics</h2><ul>");
            for item in &self.net_graph.diagnostics {
                w.text_element("li", item);
            }
            w.raw("</ul>");
        }

        w.raw("<h2>Nodes</h2><ul>");
        for node in &self.net_graph.nodes {
            w.open_tag("li");
            w.text(&node.node_fqn);
            w.raw(&format!(" (level={}) consumes=[", node.topo_level));
            w.text(&join_or_dash(&node.consumes));
            w.raw("] produces=[");
            w.text(&join_or_dash(&node.produces));
            w.raw("]");
            w.close_tag("li");
        }
        w.raw("</ul>");

        w.raw("<h2>Edges</h2><ul>");
        for edge in &self.net_graph.edges {
            w.open_tag("li");
            w.text(&edge.from);
            w.raw(" -> ");
            w.text(&edge.to);
            w.raw(&format!(" ({:?}", edge.kind));
            if let Some(label) = &edge.label {
                w.raw(", label=");
                w.text(label);
            }
            w.raw(")");
            w.close_tag("li");
        }
        w.raw("</ul>");

        w.raw("</body></html>");
        w.into_string()
    }
}

fn build_registration_graph(
    plugin_registry: &PluginRegistry,
    node_registry: &NodeRegistry,
) -> RegisteredGraph {
    let mut nodes = node_registry
        .iter()
        .map(|(_, node)| RegisteredNodeGraph {
            node_fqn: node.node_fqn.clone(),
            plugin_path: node.plugin_path.clone(),
            node_id: node.node_id.clone(),
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.node_fqn.cmp(&right.node_fqn));

    let mut node_count_by_plugin = BTreeMap::new();
    for node in &nodes {
        *node_count_by_plugin
            .entry(node.plugin_path.clone())
            .or_insert(0_usize) += 1;
    }

    let mut plugins = plugin_registry
        .iter()
        .map(|(_, plugin)| RegisteredPluginGraph {
            plugin_path: plugin.plugin_path.clone(),
            parent: plugin.parent.clone(),
            required: plugin.required,
            load_result: plugin.load_result.clone(),
            node_count: node_count_by_plugin
                .get(&plugin.plugin_path)
                .copied()
                .unwrap_or_default(),
            depth: plugin.plugin_path.matches('/').count(),
        })
        .collect::<Vec<_>>();
    plugins.sort_by(|left, right| left.plugin_path.cmp(&right.plugin_path));

    let mut edges = Vec::new();
    for plugin in &plugins {
        if let Some(parent) = &plugin.parent {
            edges.push(RegisteredGraphEdge {
                from: parent.clone(),
                to: plugin.plugin_path.clone(),
                kind: RegisteredGraphEdgeKind::PluginChild,
            });
        }
    }
    for node in &nodes {
        edges.push(RegisteredGraphEdge {
            from: node.plugin_path.clone(),
            to: node.node_fqn.clone(),
            kind: RegisteredGraphEdgeKind::PluginNode,
        });
    }

    RegisteredGraph {
        plugins,
        nodes,
        edges,
    }
}

fn build_registered_net(
    plugin_registry: &PluginRegistry,
    node_registry: &NodeRegistry,
) -> RegisteredNet {
    let mut meta = BTreeMap::<String, (String, String, Vec<String>, Vec<String>, NodeType)>::new();
    let mut producers_by_output = BTreeMap::<String, Vec<String>>::new();

    for (_, node) in node_registry.iter() {
        let Some(plugin) = plugin_registry.get(&node.plugin_path) else {
            continue;
        };
        let Some(docs) = &plugin.docs else {
            continue;
        };
        let Some(node_doc) = docs.nodes.iter().find(|doc| doc.id == node.node_id) else {
            continue;
        };

        let consumes = schema_property_names(&node_doc.input_schema);
        let produces = infer_outputs(node_doc);

        for output in &produces {
            producers_by_output
                .entry(output.clone())
                .or_default()
                .push(node.node_fqn.clone());
        }

        meta.insert(
            node.node_fqn.clone(),
            (
                node.plugin_path.clone(),
                node.node_id.clone(),
                consumes,
                produces,
                node_doc.node_type,
            ),
        );
    }

    if meta.is_empty() {
        return RegisteredNet::default();
    }

    let mut diagnostics = Vec::new();
    let mut edges = Vec::new();

    for (consumer_fqn, (_, _, consumes, _, _)) in &meta {
        for input in consumes {
            let mut candidates = producers_by_output
                .get(input)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|producer| producer != consumer_fqn)
                .collect::<Vec<_>>();
            candidates.sort();
            candidates.dedup();

            if candidates.is_empty() {
                continue;
            }
            if candidates.len() > 1 {
                // P1-50: multi-producer edges silently picked candidates[0]
                // (alphabetical order) before, which meant the runtime
                // rendered a graph structurally different from what any
                // author expected. Sort so the choice is at least
                // deterministic across builds, and emit the pick verbatim
                // as a diagnostic — callers (kernel status, HTML render)
                // surface these upstream.
                candidates.sort();
                diagnostics.push(format!(
                    "registered-net multi-producer for input `{input}` of {consumer_fqn}: candidates=[{}] chosen={} (sort-stable)",
                    candidates.join(", "),
                    candidates[0],
                ));
            }

            let chosen = candidates[0].clone();
            edges.push(RegisteredNetEdge {
                from: chosen,
                to: consumer_fqn.clone(),
                kind: RegisteredNetEdgeKind::Data,
                label: Some(input.clone()),
            });
        }
    }

    edges.sort_by(|left, right| {
        left.from
            .cmp(&right.from)
            .then_with(|| left.to.cmp(&right.to))
            .then_with(|| left.label.cmp(&right.label))
    });
    edges.dedup_by(|left, right| {
        left.from == right.from
            && left.to == right.to
            && left.label == right.label
            && std::mem::discriminant(&left.kind) == std::mem::discriminant(&right.kind)
    });

    let levels = topo_levels(meta.keys().cloned().collect(), &edges, &mut diagnostics);

    let mut nodes = meta
        .into_iter()
        .map(
            |(node_fqn, (plugin_path, node_id, consumes, produces, node_type))| RegisteredNetNode {
                topo_level: levels.get(&node_fqn).copied().unwrap_or(0),
                node_fqn,
                plugin_path,
                node_id,
                consumes,
                produces,
                node_type,
            },
        )
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| {
        left.topo_level
            .cmp(&right.topo_level)
            .then_with(|| left.node_fqn.cmp(&right.node_fqn))
    });

    for diagnostic in &diagnostics {
        eprintln!("[registered-net] {diagnostic}");
    }

    RegisteredNet {
        nodes,
        edges,
        diagnostics,
    }
}

fn topo_levels(
    node_ids: Vec<String>,
    edges: &[RegisteredNetEdge],
    diagnostics: &mut Vec<String>,
) -> BTreeMap<String, usize> {
    let mut indegree = BTreeMap::<String, usize>::new();
    let mut outgoing = BTreeMap::<String, Vec<String>>::new();

    for node_id in &node_ids {
        indegree.insert(node_id.clone(), 0);
        outgoing.insert(node_id.clone(), Vec::new());
    }

    for edge in edges {
        if !matches!(
            edge.kind,
            RegisteredNetEdgeKind::Data | RegisteredNetEdgeKind::Control
        ) {
            continue;
        }
        if !indegree.contains_key(&edge.from) || !indegree.contains_key(&edge.to) {
            continue;
        }
        *indegree.entry(edge.to.clone()).or_insert(0) += 1;
        outgoing
            .entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }

    let mut levels = BTreeMap::<String, usize>::new();
    let mut queue = indegree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(node, _)| node.clone())
        .collect::<VecDeque<_>>();

    while let Some(node) = queue.pop_front() {
        let level = levels.get(&node).copied().unwrap_or(0);
        if let Some(nexts) = outgoing.get(&node) {
            for next in nexts {
                let entry = levels.entry(next.clone()).or_insert(0);
                if *entry < level + 1 {
                    *entry = level + 1;
                }
                if let Some(deg) = indegree.get_mut(next) {
                    if *deg > 0 {
                        *deg -= 1;
                    }
                    if *deg == 0 {
                        queue.push_back(next.clone());
                    }
                }
            }
        }
    }

    let unresolved = indegree
        .iter()
        .filter(|(_, deg)| **deg > 0)
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    if !unresolved.is_empty() {
        diagnostics.push(format!(
            "cycle-like dependencies detected among: {}",
            unresolved.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    for node_id in node_ids {
        // P1-51: cycle-participants used to fall through to `or_insert(0)`,
        // which made them indistinguishable from real roots in downstream
        // consumers (HTML render, engine ordering). Give them a distinct
        // sentinel — `usize::MAX` — so callers can filter or highlight
        // them, and the sort in `build_execution_net` puts them last.
        let default_level = if unresolved.contains(&node_id) {
            usize::MAX
        } else {
            0
        };
        levels.entry(node_id).or_insert(default_level);
    }

    levels
}

fn infer_outputs(node_doc: &NodeDoc) -> Vec<String> {
    schema_property_names(&node_doc.output_schema)
        .into_iter()
        .filter(|name| name != "error")
        .collect()
}

fn schema_property_names(schema: &Value) -> Vec<String> {
    let Some(properties) = schema.get("properties").and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut names = properties.keys().cloned().collect::<Vec<_>>();
    names.sort();
    names
}

fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::ArtifactKind;
    use crate::plugin::registry::{NodeRegistry, PluginRegistry};
    use cordis_plugin_sdk::{node_doc, plugin_docs, AbiFingerprint, NodeDoc, PluginDocs};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn doc_with_schema(id: &str, inputs: &[&str], outputs: &[&str]) -> NodeDoc {
        let props = |names: &[&str]| {
            let mut map = serde_json::Map::new();
            for name in names {
                map.insert((*name).to_string(), serde_json::json!({"type": "string"}));
            }
            serde_json::json!({ "type": "object", "properties": map })
        };
        node_doc(id, "n", props(inputs), props(outputs), &[], &[])
    }

    fn insert_plugin(
        registry: &PluginRegistry,
        plugin_path: &str,
        parent: Option<&str>,
        docs: PluginDocs,
    ) {
        registry.insert_loaded(
            plugin_path.to_string(),
            parent.map(ToString::to_string),
            true,
            BTreeSet::new(),
            docs,
            PathBuf::from("/tmp/a.json"),
            ArtifactKind::Json,
            AbiFingerprint::current_build("crate", "api"),
            None,
        );
    }

    // Build a linear chain: producer node emits `x`, consumer node reads `x`.
    fn chain_registries() -> (PluginRegistry, NodeRegistry) {
        let plugins = PluginRegistry::default();
        let docs = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![
                doc_with_schema("producer", &[], &["x"]),
                doc_with_schema("consumer", &["x"], &[]),
            ],
            None,
        );
        insert_plugin(&plugins, "root/p", None, docs.clone());
        let mut nodes = NodeRegistry::default();
        nodes.register_from_docs("root/p", &docs).expect("register");
        (plugins, nodes)
    }

    #[test]
    fn accessors_expose_built_graphs() {
        let (plugins, nodes) = chain_registries();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        // registration graph: 1 plugin, 2 nodes.
        assert_eq!(reg.graph().plugins.len(), 1);
        assert_eq!(reg.graph().nodes.len(), 2);
        // one plugin->node edge per node, no plugin-child edge (no parent).
        assert_eq!(reg.graph().edges.len(), 2);
        // net graph: 2 nodes, 1 data edge producer->consumer.
        assert_eq!(reg.net().nodes.len(), 2);
        assert_eq!(reg.net().edges.len(), 1);
        let edge = &reg.net().edges[0];
        assert_eq!(edge.from, "root/p::producer");
        assert_eq!(edge.to, "root/p::consumer");
        assert_eq!(edge.label.as_deref(), Some("x"));
        assert!(matches!(edge.kind, RegisteredNetEdgeKind::Data));
        // topo levels: producer at 0, consumer at 1.
        let consumer = reg
            .net()
            .nodes
            .iter()
            .find(|n| n.node_fqn == "root/p::consumer")
            .unwrap();
        assert_eq!(consumer.topo_level, 1);
    }

    #[test]
    fn plugin_child_edge_recorded_when_parent_present() {
        let plugins = PluginRegistry::default();
        insert_plugin(
            &plugins,
            "root",
            None,
            plugin_docs("r", "root", "0.1.0", None, vec![], None),
        );
        insert_plugin(
            &plugins,
            "root/child",
            Some("root"),
            plugin_docs(
                "c",
                "root/child",
                "0.1.0",
                None,
                vec![doc_with_schema("n0", &[], &[])],
                None,
            ),
        );
        let mut nodes = NodeRegistry::default();
        let child_docs = plugins.get("root/child").unwrap().docs.unwrap();
        nodes.register_from_docs("root/child", &child_docs).unwrap();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        assert!(reg.graph().edges.iter().any(|e| matches!(
            e.kind,
            RegisteredGraphEdgeKind::PluginChild
        ) && e.from == "root"
            && e.to == "root/child"));
    }

    #[test]
    fn handle_get_json_routes() {
        let (plugins, nodes) = chain_registries();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        let v = reg.handle_get_json("/graphs/registered-nodes").unwrap();
        assert!(v["plugins"].is_array());
        let v = reg.handle_get_json("/graphs/registered-net").unwrap();
        assert!(v["nodes"].is_array());
        assert!(matches!(
            reg.handle_get_json("/graphs/unknown"),
            Err(RuntimeError::InvalidDocsRoute { .. })
        ));
    }

    #[test]
    fn handle_get_html_routes() {
        let (plugins, nodes) = chain_registries();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        let html = reg
            .handle_get_html("/graphs/registered-nodes.html")
            .unwrap();
        assert!(html.contains("Registered Nodes Graph"));
        assert!(html.contains("root/p::producer"));
        let html = reg.handle_get_html("/graphs/registered-net.html").unwrap();
        assert!(html.contains("Registered Net"));
        assert!(html.contains(" -> ")); // edge arrow written via raw()
        assert!(html.contains("label="));
        assert!(matches!(
            reg.handle_get_html("/graphs/unknown.html"),
            Err(RuntimeError::InvalidDocsRoute { .. })
        ));
    }

    #[test]
    fn render_net_html_shows_dash_for_empty_consumes_and_produces() {
        // Single isolated node with no inputs/outputs -> consumes/produces "-".
        let plugins = PluginRegistry::default();
        let docs = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![doc_with_schema("lonely", &[], &[])],
            None,
        );
        insert_plugin(&plugins, "root/p", None, docs.clone());
        let mut nodes = NodeRegistry::default();
        nodes.register_from_docs("root/p", &docs).unwrap();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        let html = reg.render_registered_net_html();
        assert!(html.contains("consumes=[-]"));
        assert!(html.contains("produces=[-]"));
    }

    #[test]
    fn multi_producer_emits_diagnostic() {
        // Two producers of `x`, one consumer of `x`.
        let plugins = PluginRegistry::default();
        let docs = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![
                doc_with_schema("prod_a", &[], &["x"]),
                doc_with_schema("prod_b", &[], &["x"]),
                doc_with_schema("consumer", &["x"], &[]),
            ],
            None,
        );
        insert_plugin(&plugins, "root/p", None, docs.clone());
        let mut nodes = NodeRegistry::default();
        nodes.register_from_docs("root/p", &docs).unwrap();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        assert!(
            reg.net()
                .diagnostics
                .iter()
                .any(|d| d.contains("multi-producer") && d.contains("`x`")),
            "diagnostics: {:?}",
            reg.net().diagnostics
        );
        // Deterministic pick: alphabetically first producer (prod_a).
        let edge = reg
            .net()
            .edges
            .iter()
            .find(|e| e.to == "root/p::consumer")
            .unwrap();
        assert_eq!(edge.from, "root/p::prod_a");
        // Diagnostics surface in the net HTML.
        let html = reg.render_registered_net_html();
        assert!(html.contains("Net diagnostics"));
    }

    #[test]
    fn cycle_dependency_detected_and_marked() {
        // a consumes y (produced by b); b consumes x (produced by a) -> cycle.
        let plugins = PluginRegistry::default();
        let docs = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![
                doc_with_schema("a", &["y"], &["x"]),
                doc_with_schema("b", &["x"], &["y"]),
            ],
            None,
        );
        insert_plugin(&plugins, "root/p", None, docs.clone());
        let mut nodes = NodeRegistry::default();
        nodes.register_from_docs("root/p", &docs).unwrap();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        assert!(
            reg.net()
                .diagnostics
                .iter()
                .any(|d| d.contains("cycle-like")),
            "diagnostics: {:?}",
            reg.net().diagnostics
        );
        // Cycle participants get the usize::MAX sentinel level.
        assert!(reg.net().nodes.iter().all(|n| n.topo_level == usize::MAX));
    }

    #[test]
    fn empty_registries_produce_default_net() {
        let plugins = PluginRegistry::default();
        let nodes = NodeRegistry::default();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        assert!(reg.net().nodes.is_empty());
        assert!(reg.net().edges.is_empty());
        assert!(reg.net().diagnostics.is_empty());
    }

    #[test]
    fn node_without_docs_entry_is_skipped_in_net() {
        // Node registered whose id is absent from the plugin docs nodes list.
        // build_registered_net skips it (no matching node_doc).
        let plugins = PluginRegistry::default();
        // docs only declare `known`; we register an extra `ghost` node manually.
        let docs = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![doc_with_schema("known", &[], &[])],
            None,
        );
        insert_plugin(&plugins, "root/p", None, docs.clone());
        let mut nodes = NodeRegistry::default();
        nodes.register_from_docs("root/p", &docs).unwrap();
        // Register a second plugin path that has NO entry in plugin_registry,
        // so the `plugin_registry.get(...)` else-branch (continue) is hit.
        let orphan_docs = plugin_docs(
            "o",
            "orphan",
            "0.1.0",
            None,
            vec![doc_with_schema("g", &[], &[])],
            None,
        );
        nodes.register_from_docs("orphan", &orphan_docs).unwrap();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        // Only the `known` node survives into the net.
        assert_eq!(reg.net().nodes.len(), 1);
        assert_eq!(reg.net().nodes[0].node_fqn, "root/p::known");
    }

    #[test]
    fn error_output_property_is_not_treated_as_produced() {
        // infer_outputs filters out the reserved `error` property.
        let plugins = PluginRegistry::default();
        let docs = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![doc_with_schema("n", &[], &["error"])],
            None,
        );
        insert_plugin(&plugins, "root/p", None, docs.clone());
        let mut nodes = NodeRegistry::default();
        nodes.register_from_docs("root/p", &docs).unwrap();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        assert!(reg.net().nodes[0].produces.is_empty());
    }

    #[test]
    fn join_or_dash_behaviour() {
        assert_eq!(join_or_dash(&[]), "-");
        assert_eq!(join_or_dash(&["a".to_string(), "b".to_string()]), "a, b");
    }

    #[test]
    fn node_whose_plugin_has_no_docs_is_skipped_in_net() {
        // Register a node from docs, then overwrite the plugin entry with an
        // *unavailable* record whose `docs` is None. build_registered_net then
        // hits the `let Some(docs) = &plugin.docs else { continue }` skip.
        let plugins = PluginRegistry::default();
        let docs = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![doc_with_schema("n0", &[], &[])],
            None,
        );
        let mut nodes = NodeRegistry::default();
        nodes.register_from_docs("root/p", &docs).unwrap();
        // Overwrite "root/p" with a docs-less unavailable entry.
        plugins.insert_unavailable(
            "root/p".to_string(),
            None,
            true,
            BTreeSet::new(),
            crate::core::models::PluginUnavailableReason::ArtifactMissing,
            Vec::new(),
        );
        assert!(plugins.get("root/p").unwrap().docs.is_none());
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        // The node's plugin has no docs -> node dropped from the net.
        assert!(reg.net().nodes.is_empty());
    }

    #[test]
    fn node_absent_from_plugin_docs_is_skipped_in_net() {
        // Register node "ghost" from an initial docs set, then re-insert the
        // plugin with docs that only declare "known". The net builder looks up
        // each registered node in the (now-changed) plugin docs; "ghost" is
        // absent, exercising the `find(...) else { continue }` skip on the
        // node-doc lookup.
        let plugins = PluginRegistry::default();
        let docs_with_ghost = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![doc_with_schema("ghost", &[], &[])],
            None,
        );
        let mut nodes = NodeRegistry::default();
        nodes
            .register_from_docs("root/p", &docs_with_ghost)
            .unwrap();
        // Re-insert the plugin with different docs that lack "ghost".
        let docs_known_only = plugin_docs(
            "p",
            "root/p",
            "0.1.0",
            None,
            vec![doc_with_schema("known", &[], &[])],
            None,
        );
        insert_plugin(&plugins, "root/p", None, docs_known_only);
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        // "ghost" is registered as a node but absent from the plugin docs, so
        // build_registered_net skips it -> empty net.
        assert!(reg.net().nodes.is_empty());
    }

    #[test]
    fn schema_without_properties_key_yields_no_names() {
        // A node whose input/output schema is not an object-with-properties
        // (here a bare `{"type":"string"}`) makes `schema_property_names` take
        // its early-return `Vec::new()` path: no consumes / produces inferred.
        let plugins = PluginRegistry::default();
        let bare = node_doc(
            "scalar",
            "n",
            serde_json::json!({ "type": "string" }),
            serde_json::json!({ "type": "string" }),
            &[],
            &[],
        );
        let docs = plugin_docs("p", "root/p", "0.1.0", None, vec![bare], None);
        insert_plugin(&plugins, "root/p", None, docs.clone());
        let mut nodes = NodeRegistry::default();
        nodes.register_from_docs("root/p", &docs).unwrap();
        let reg = GraphRegistry::from_registries(&plugins, &nodes);
        assert_eq!(reg.net().nodes.len(), 1);
        assert!(reg.net().nodes[0].consumes.is_empty());
        assert!(reg.net().nodes[0].produces.is_empty());
    }
}
