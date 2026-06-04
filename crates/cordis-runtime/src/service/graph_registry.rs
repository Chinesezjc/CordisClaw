use crate::core::error::RuntimeError;
use crate::core::models::{NodeDoc, PluginLoadResult};
use crate::plugin::registry::{NodeRegistry, PluginRegistry};
use crate::service::html_render::HtmlWriter;
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
    let mut meta = BTreeMap::<String, (String, String, Vec<String>, Vec<String>)>::new();
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
            ),
        );
    }

    if meta.is_empty() {
        return RegisteredNet::default();
    }

    let mut diagnostics = Vec::new();
    let mut edges = Vec::new();

    for (consumer_fqn, (_, _, consumes, _)) in &meta {
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
                diagnostics.push(format!(
                    "input {input} for {consumer_fqn} has multiple producers: {} (choosing first)",
                    candidates.join(", ")
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
            |(node_fqn, (plugin_path, node_id, consumes, produces))| RegisteredNetNode {
                topo_level: levels.get(&node_fqn).copied().unwrap_or(0),
                node_fqn,
                plugin_path,
                node_id,
                consumes,
                produces,
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
            unresolved.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    for node_id in node_ids {
        levels.entry(node_id).or_insert(0);
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
