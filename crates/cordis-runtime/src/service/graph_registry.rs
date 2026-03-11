use crate::core::error::RuntimeError;
use crate::core::models::{NodeDoc, PluginLoadResult};
use crate::execution::dag::{
    build_dag, DagBuildPolicy, DagEdgeKind, DagGraph, DagInputSpec, DagNodeSpec,
};
use crate::plugin::registry::{NodeRegistry, PluginRegistry};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const PLUGIN_CARD_WIDTH: usize = 320;
const PLUGIN_CARD_GAP_X: usize = 64;
const PLUGIN_CARD_GAP_Y: usize = 28;
const PLUGIN_CARD_X_OFFSET: usize = 40;
const PLUGIN_CARD_Y_OFFSET: usize = 40;
const NODE_ROW_HEIGHT: usize = 28;
const PLUGIN_CARD_MIN_HEIGHT: usize = 108;

const DAG_CARD_WIDTH: usize = 320;
const DAG_CARD_GAP_X: usize = 72;
const DAG_CARD_GAP_Y: usize = 32;
const DAG_CARD_X_OFFSET: usize = 40;
const DAG_CARD_Y_OFFSET: usize = 40;
const DAG_CARD_MIN_HEIGHT: usize = 132;
const DAG_LINE_ROW_HEIGHT: usize = 22;

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
pub struct RegisteredDag {
    pub nodes: Vec<RegisteredDagNode>,
    pub edges: Vec<RegisteredDagEdge>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredDagNode {
    pub node_fqn: String,
    pub plugin_path: String,
    pub node_id: String,
    pub consumes: Vec<String>,
    pub produces: Vec<String>,
    pub topo_level: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredDagEdge {
    pub from: String,
    pub to: String,
    pub kind: RegisteredDagEdgeKind,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredDagEdgeKind {
    Data,
    Control,
}

#[derive(Debug, Default, Clone)]
pub struct GraphRegistry {
    registration_graph: RegisteredGraph,
    dag_graph: RegisteredDag,
}

impl GraphRegistry {
    pub fn from_registries(plugin_registry: &PluginRegistry, node_registry: &NodeRegistry) -> Self {
        let registration_graph = build_registration_graph(plugin_registry, node_registry);
        let dag_graph = build_registered_dag(plugin_registry, node_registry);
        Self {
            registration_graph,
            dag_graph,
        }
    }

    pub fn graph(&self) -> &RegisteredGraph {
        &self.registration_graph
    }

    pub fn dag(&self) -> &RegisteredDag {
        &self.dag_graph
    }

    pub fn handle_get_json(&self, path: &str) -> Result<Value, RuntimeError> {
        match path {
            "/graphs/registered-nodes" => {
                serde_json::to_value(&self.registration_graph).map_err(|err| RuntimeError::Invariant {
                    message: format!("serialize registered graph failed: {err}"),
                })
            }
            "/graphs/registered-dag" => {
                serde_json::to_value(&self.dag_graph).map_err(|err| RuntimeError::Invariant {
                    message: format!("serialize registered dag failed: {err}"),
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
            "/graphs/registered-dag.html" => Ok(self.render_registered_dag_html()),
            _ => Err(RuntimeError::InvalidDocsRoute {
                path: path.to_string(),
            }),
        }
    }

    pub fn render_registered_nodes_html(&self) -> String {
        let layout = layout_plugins(&self.registration_graph);
        let width = layout.canvas_width;
        let height = layout.canvas_height;

        let mut html = String::new();
        start_html(
            &mut html,
            "Registered Nodes Graph",
            "Graph of currently registered plugins and nodes. Parent-child edges show plugin nesting; node rows show registrations inside each plugin.",
        );
        html.push_str(&format!("<div class=\"graph\" style=\"min-width:{}px;min-height:{}px\">", width, height));
        html.push_str(&format!("<svg width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" xmlns=\"http://www.w3.org/2000/svg\">", width, height, width, height));
        html.push_str(svg_defs());
        for edge in &self.registration_graph.edges {
            if !matches!(edge.kind, RegisteredGraphEdgeKind::PluginChild) {
                continue;
            }
            let Some(from) = layout.cards.get(&edge.from) else {
                continue;
            };
            let Some(to) = layout.cards.get(&edge.to) else {
                continue;
            };
            append_curve(&mut html, from.x + from.width, from.y + from.height / 2, to.x, to.y + to.height / 2);
        }
        html.push_str("</svg>");

        for plugin in &self.registration_graph.plugins {
            let Some(card) = layout.cards.get(&plugin.plugin_path) else {
                continue;
            };
            let nodes = self
                .registration_graph
                .nodes
                .iter()
                .filter(|node| node.plugin_path == plugin.plugin_path)
                .collect::<Vec<_>>();
            let load_label = match &plugin.load_result {
                PluginLoadResult::Loaded => "loaded".to_string(),
                PluginLoadResult::Unavailable(reason) => format!("unavailable: {:?}", reason),
            };
            let load_class = load_class(&plugin.load_result);
            append_card_start(&mut html, card.x, card.y, card.width, card.height);
            html.push_str(&format!("<h2>{}</h2>", escape_html(&plugin.plugin_path)));
            html.push_str(&format!(
                "<div class=\"meta\"><span>{}</span><span class=\"badge {}\">{}</span></div>",
                if plugin.required { "required" } else { "optional" },
                load_class,
                escape_html(&load_label)
            ));
            html.push_str(&format!(
                "<div class=\"meta\"><span>depth {}</span><span>{} node(s)</span></div>",
                plugin.depth,
                plugin.node_count
            ));
            if nodes.is_empty() {
                html.push_str("<div class=\"empty\">No registered nodes</div>");
            } else {
                html.push_str("<div class=\"nodes\">");
                for node in nodes {
                    html.push_str(&format!(
                        "<div class=\"node\">{}<small>{}</small></div>",
                        escape_html(&node.node_id),
                        escape_html(&node.node_fqn)
                    ));
                }
                html.push_str("</div>");
            }
            html.push_str("</div>");
        }

        finish_html(&mut html);
        html
    }

    pub fn render_registered_dag_html(&self) -> String {
        let layout = layout_dag(&self.dag_graph);
        let width = layout.canvas_width;
        let height = layout.canvas_height;

        let mut html = String::new();
        start_html(
            &mut html,
            "Registered DAG",
            "DAG inferred from registered node docs. Data edges are built from input/output schema property names and laid out by topological level.",
        );
        html.push_str(&format!("<div class=\"graph\" style=\"min-width:{}px;min-height:{}px\">", width, height));
        html.push_str(&format!("<svg width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" xmlns=\"http://www.w3.org/2000/svg\">", width, height, width, height));
        html.push_str(svg_defs());
        for edge in &self.dag_graph.edges {
            let Some(from) = layout.cards.get(&edge.from) else {
                continue;
            };
            let Some(to) = layout.cards.get(&edge.to) else {
                continue;
            };
            append_curve(&mut html, from.x + from.width, from.y + from.height / 2, to.x, to.y + to.height / 2);
            if let Some(label) = &edge.label {
                let mid_x = (from.x + from.width + to.x) / 2;
                let mid_y = (from.y + from.height / 2 + to.y + to.height / 2) / 2;
                html.push_str(&format!(
                    "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" font-size=\"12\" fill=\"#665f53\" font-family=\"ui-monospace,SFMono-Regular,Menlo,monospace\">{}</text>",
                    mid_x,
                    mid_y.saturating_sub(6),
                    escape_html(label)
                ));
            }
        }
        html.push_str("</svg>");

        if !self.dag_graph.diagnostics.is_empty() {
            html.push_str("<div class=\"card\" style=\"left:24px;top:24px;width:calc(100% - 96px);height:auto;background:#fff1ef;border-color:#e0b7af;\">");
            html.push_str("<h2>DAG diagnostics</h2><div class=\"nodes\">");
            for item in &self.dag_graph.diagnostics {
                html.push_str(&format!("<div class=\"node\">{}</div>", escape_html(item)));
            }
            html.push_str("</div></div>");
        }

        for node in &self.dag_graph.nodes {
            let Some(card) = layout.cards.get(&node.node_fqn) else {
                continue;
            };
            append_card_start(&mut html, card.x, card.y, card.width, card.height);
            html.push_str(&format!("<h2>{}</h2>", escape_html(&node.node_id)));
            html.push_str(&format!(
                "<div class=\"meta\"><span>{}</span><span class=\"badge loaded\">level {}</span></div>",
                escape_html(&node.plugin_path),
                node.topo_level,
            ));
            html.push_str(&format!(
                "<div class=\"meta\"><span>consumes {}</span><span>produces {}</span></div>",
                node.consumes.len(),
                node.produces.len(),
            ));
            html.push_str(&format!(
                "<div class=\"nodes\"><div class=\"node\">{}<small>{}</small></div>",
                escape_html(&node.node_fqn),
                escape_html(&format!("plugin={} ", node.plugin_path))
            ));
            html.push_str(&format!(
                "<div class=\"node\">Consumes<small>{}</small></div>",
                escape_html(&join_or_dash(&node.consumes))
            ));
            html.push_str(&format!(
                "<div class=\"node\">Produces<small>{}</small></div>",
                escape_html(&join_or_dash(&node.produces))
            ));
            html.push_str("</div></div>");
        }

        finish_html(&mut html);
        html
    }
}

#[derive(Debug)]
struct GraphLayout {
    cards: BTreeMap<String, GraphCard>,
    canvas_width: usize,
    canvas_height: usize,
}

#[derive(Debug)]
struct GraphCard {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

fn build_registration_graph(plugin_registry: &PluginRegistry, node_registry: &NodeRegistry) -> RegisteredGraph {
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

fn build_registered_dag(plugin_registry: &PluginRegistry, node_registry: &NodeRegistry) -> RegisteredDag {
    let mut specs = Vec::new();
    let mut meta = BTreeMap::<String, (String, String, Vec<String>, Vec<String>)>::new();

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

        let (consumes, consume_names) = infer_inputs(node_doc);
        let produces = infer_outputs(node_doc);
        meta.insert(
            node.node_fqn.clone(),
            (
                node.plugin_path.clone(),
                node.node_id.clone(),
                consume_names,
                produces.clone(),
            ),
        );
        specs.push(DagNodeSpec {
            node_id: node.node_fqn.clone(),
            priority: 0,
            consumes,
            produces,
            control_deps: Vec::new(),
        });
    }

    if specs.is_empty() {
        return RegisteredDag::default();
    }

    match build_dag(
        specs,
        DagBuildPolicy {
            require_explicit_binding_for_multi_producer: false,
        },
    ) {
        Ok(graph) => map_dag(graph, meta),
        Err(err) => RegisteredDag {
            nodes: meta
                .into_iter()
                .map(|(node_fqn, (plugin_path, node_id, consumes, produces))| RegisteredDagNode {
                    node_fqn,
                    plugin_path,
                    node_id,
                    consumes,
                    produces,
                    topo_level: 0,
                })
                .collect(),
            edges: Vec::new(),
            diagnostics: vec![err.to_string()],
        },
    }
}

fn map_dag(
    graph: DagGraph,
    meta: BTreeMap<String, (String, String, Vec<String>, Vec<String>)>,
) -> RegisteredDag {
    let levels = topo_levels(&graph);
    let mut nodes = meta
        .into_iter()
        .map(|(node_fqn, (plugin_path, node_id, consumes, produces))| RegisteredDagNode {
            topo_level: levels.get(&node_fqn).copied().unwrap_or(0),
            node_fqn,
            plugin_path,
            node_id,
            consumes,
            produces,
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| {
        left.topo_level
            .cmp(&right.topo_level)
            .then_with(|| left.node_fqn.cmp(&right.node_fqn))
    });

    let edges = graph
        .edges
        .into_iter()
        .map(|edge| RegisteredDagEdge {
            from: edge.from,
            to: edge.to,
            label: match &edge.kind {
                DagEdgeKind::Data { input_type } => Some(input_type.clone()),
                DagEdgeKind::Control => None,
            },
            kind: match edge.kind {
                DagEdgeKind::Data { .. } => RegisteredDagEdgeKind::Data,
                DagEdgeKind::Control => RegisteredDagEdgeKind::Control,
            },
        })
        .collect();

    RegisteredDag {
        nodes,
        edges,
        diagnostics: Vec::new(),
    }
}

fn infer_inputs(node_doc: &NodeDoc) -> (Vec<DagInputSpec>, Vec<String>) {
    let required = required_fields(&node_doc.input_schema);
    let names = schema_property_names(&node_doc.input_schema);
    let inputs = names
        .iter()
        .map(|name| DagInputSpec {
            input_type: name.clone(),
            // Display DAGs treat unmatched schema inputs as external roots
            // instead of hard-failing the visualization.
            required: false,
            explicit_producer: None,
        })
        .collect::<Vec<_>>();
    let _ = required;
    (inputs, names)
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

fn required_fields(schema: &Value) -> BTreeSet<String> {
    schema
        .get("required")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn topo_levels(graph: &DagGraph) -> BTreeMap<String, usize> {
    let mut indegree = BTreeMap::<String, usize>::new();
    let mut outgoing = BTreeMap::<String, Vec<String>>::new();

    for node_id in graph.nodes.keys() {
        indegree.insert(node_id.clone(), 0);
        outgoing.insert(node_id.clone(), Vec::new());
    }

    for edge in &graph.edges {
        *indegree.entry(edge.to.clone()).or_insert(0) += 1;
        outgoing.entry(edge.from.clone()).or_default().push(edge.to.clone());
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
                let next_level = levels.get(next).copied().unwrap_or(0).max(level + 1);
                levels.insert(next.clone(), next_level);
                if let Some(indegree_value) = indegree.get_mut(next) {
                    *indegree_value = indegree_value.saturating_sub(1);
                    if *indegree_value == 0 {
                        queue.push_back(next.clone());
                    }
                }
            }
        }
    }

    for node_id in graph.nodes.keys() {
        levels.entry(node_id.clone()).or_insert(0);
    }

    levels
}

fn layout_plugins(graph: &RegisteredGraph) -> GraphLayout {
    let mut cards = BTreeMap::new();
    let mut y_by_depth = BTreeMap::<usize, usize>::new();
    let max_depth = graph.plugins.iter().map(|plugin| plugin.depth).max().unwrap_or(0);

    for plugin in &graph.plugins {
        let x = PLUGIN_CARD_X_OFFSET + plugin.depth * (PLUGIN_CARD_WIDTH + PLUGIN_CARD_GAP_X);
        let next_y = y_by_depth.entry(plugin.depth).or_insert(PLUGIN_CARD_Y_OFFSET);
        let height = (88 + plugin.node_count * NODE_ROW_HEIGHT).max(PLUGIN_CARD_MIN_HEIGHT);
        cards.insert(
            plugin.plugin_path.clone(),
            GraphCard {
                x,
                y: *next_y,
                width: PLUGIN_CARD_WIDTH,
                height,
            },
        );
        *next_y += height + PLUGIN_CARD_GAP_Y;
    }

    let canvas_width = PLUGIN_CARD_X_OFFSET * 2
        + (max_depth + 1) * PLUGIN_CARD_WIDTH
        + max_depth * PLUGIN_CARD_GAP_X;
    let canvas_height = y_by_depth
        .values()
        .copied()
        .max()
        .unwrap_or(PLUGIN_CARD_Y_OFFSET + PLUGIN_CARD_MIN_HEIGHT)
        + PLUGIN_CARD_Y_OFFSET;

    GraphLayout {
        cards,
        canvas_width,
        canvas_height,
    }
}

fn layout_dag(dag: &RegisteredDag) -> GraphLayout {
    let mut cards = BTreeMap::new();
    let mut y_by_level = BTreeMap::<usize, usize>::new();
    let max_level = dag.nodes.iter().map(|node| node.topo_level).max().unwrap_or(0);

    for node in &dag.nodes {
        let x = DAG_CARD_X_OFFSET + node.topo_level * (DAG_CARD_WIDTH + DAG_CARD_GAP_X);
        let next_y = y_by_level.entry(node.topo_level).or_insert(DAG_CARD_Y_OFFSET);
        let line_count = 3 + node.consumes.len().max(1) + node.produces.len().max(1);
        let height = (68 + line_count * DAG_LINE_ROW_HEIGHT).max(DAG_CARD_MIN_HEIGHT);
        cards.insert(
            node.node_fqn.clone(),
            GraphCard {
                x,
                y: *next_y,
                width: DAG_CARD_WIDTH,
                height,
            },
        );
        *next_y += height + DAG_CARD_GAP_Y;
    }

    let canvas_width = DAG_CARD_X_OFFSET * 2
        + (max_level + 1) * DAG_CARD_WIDTH
        + max_level * DAG_CARD_GAP_X;
    let canvas_height = y_by_level
        .values()
        .copied()
        .max()
        .unwrap_or(DAG_CARD_Y_OFFSET + DAG_CARD_MIN_HEIGHT)
        + DAG_CARD_Y_OFFSET;

    GraphLayout {
        cards,
        canvas_width,
        canvas_height,
    }
}

fn start_html(html: &mut String, title: &str, subtitle: &str) {
    html.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">");
    html.push_str(&format!("<title>{}</title><style>", escape_html(title)));
    html.push_str("body{margin:0;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:#f4f1e8;color:#1d1b16;}\n");
    html.push_str(".page{padding:24px 24px 40px;}\n");
    html.push_str(".header{margin-bottom:16px;}\n");
    html.push_str(".header h1{margin:0 0 8px;font-size:24px;}\n");
    html.push_str(".header p{margin:0;color:#5b5448;max-width:900px;}\n");
    html.push_str(".graph{position:relative;overflow:auto;border:1px solid #cfc7b6;background:linear-gradient(180deg,#fbf8ef,#f1ebdf);border-radius:18px;box-shadow:0 24px 60px rgba(47,40,28,.08);}\n");
    html.push_str("svg{display:block;}\n");
    html.push_str(".card{position:absolute;border:1px solid #b8ad96;border-radius:16px;background:#fffdf8;box-shadow:0 12px 28px rgba(40,31,18,.08);padding:16px 16px 12px;}\n");
    html.push_str(".card h2{margin:0;font-size:15px;line-height:1.4;}\n");
    html.push_str(".meta{display:flex;justify-content:space-between;gap:8px;align-items:center;margin-top:8px;font-size:12px;color:#5b5448;}\n");
    html.push_str(".badge{display:inline-block;padding:3px 8px;border-radius:999px;font-size:11px;font-weight:700;text-transform:uppercase;letter-spacing:.04em;}\n");
    html.push_str(".loaded{background:#d9f2dc;color:#1f5d2a;} .unavailable{background:#f7d7cf;color:#8b2c1d;}\n");
    html.push_str(".nodes{margin-top:12px;padding-top:10px;border-top:1px solid #e7e0d1;}\n");
    html.push_str(".node{padding:6px 8px;border-radius:10px;background:#f5efe2;margin-top:8px;font-size:12px;}\n");
    html.push_str(".node small{display:block;color:#6a6254;margin-top:2px;}\n");
    html.push_str(".empty{margin-top:12px;font-size:12px;color:#7a7266;font-style:italic;}\n");
    html.push_str("</style></head><body><div class=\"page\">\n");
    html.push_str(&format!(
        "<div class=\"header\"><h1>{}</h1><p>{}</p></div>",
        escape_html(title),
        escape_html(subtitle)
    ));
}

fn finish_html(html: &mut String) {
    html.push_str("</div></div></body></html>");
}

fn append_curve(html: &mut String, start_x: usize, start_y: usize, end_x: usize, end_y: usize) {
    let mid_x = (start_x + end_x) / 2;
    html.push_str(&format!(
        "<path d=\"M{} {} C {} {}, {} {}, {} {}\" fill=\"none\" stroke=\"#8b826f\" stroke-width=\"2\" marker-end=\"url(#arrow)\" opacity=\"0.9\"/>",
        start_x, start_y, mid_x, start_y, mid_x, end_y, end_x, end_y
    ));
}

fn append_card_start(html: &mut String, x: usize, y: usize, width: usize, height: usize) {
    html.push_str(&format!(
        "<div class=\"card\" style=\"left:{}px;top:{}px;width:{}px;height:{}px\">",
        x,
        y,
        width.saturating_sub(34),
        height.saturating_sub(30)
    ));
}

fn svg_defs() -> &'static str {
    "<defs><marker id=\"arrow\" markerWidth=\"10\" markerHeight=\"10\" refX=\"9\" refY=\"5\" orient=\"auto\"><path d=\"M0,0 L10,5 L0,10 z\" fill=\"#8b826f\"/></marker></defs>"
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

fn load_class(load_result: &PluginLoadResult) -> &'static str {
    match load_result {
        PluginLoadResult::Loaded => "loaded",
        PluginLoadResult::Unavailable(_) => "unavailable",
    }
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
