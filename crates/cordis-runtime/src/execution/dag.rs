//! DAG build semantics and fail-fast validation.
//! This module implements:
//! - producer selection / conflict checks
//! - cycle detection with cycle path return
//! - required-input completeness checks

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagInputSpec {
    pub input_type: String,
    pub required: bool,
    pub explicit_producer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagNodeSpec {
    pub node_id: String,
    pub priority: i32,
    pub consumes: Vec<DagInputSpec>,
    pub produces: Vec<String>,
    pub control_deps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DagEdgeKind {
    Data { input_type: String },
    Control,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagEdge {
    pub from: String,
    pub to: String,
    pub kind: DagEdgeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagBuildPolicy {
    /// Conservative v1: if more than one producer exists and no explicit binding,
    /// fail fast instead of applying implicit tie-breaking.
    pub require_explicit_binding_for_multi_producer: bool,
}

impl Default for DagBuildPolicy {
    fn default() -> Self {
        Self {
            require_explicit_binding_for_multi_producer: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DagGraph {
    pub nodes: BTreeMap<String, DagNodeSpec>,
    pub edges: Vec<DagEdge>,
    pub dependencies: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DagBuildError {
    #[error("duplicate node id: {node_id}")]
    DuplicateNodeId { node_id: String },

    #[error("explicit producer not found for consumer={consumer}, input={input_type}, producer={producer}")]
    ExplicitProducerNotFound {
        consumer: String,
        input_type: String,
        producer: String,
    },

    #[error("required input missing for consumer={consumer}, input={input_type}")]
    MissingRequiredInput { consumer: String, input_type: String },

    #[error("producer conflict for consumer={consumer}, input={input_type}, candidates={producers:?}")]
    ProducerConflict {
        consumer: String,
        input_type: String,
        producers: Vec<String>,
    },

    #[error("dag cycle detected: {cycle_path:?}")]
    CycleDetected { cycle_path: Vec<String> },
}

pub fn build_dag(nodes: Vec<DagNodeSpec>, policy: DagBuildPolicy) -> Result<DagGraph, DagBuildError> {
    let mut node_map = BTreeMap::new();
    for node in nodes {
        if node_map.contains_key(&node.node_id) {
            return Err(DagBuildError::DuplicateNodeId {
                node_id: node.node_id,
            });
        }
        node_map.insert(node.node_id.clone(), node);
    }

    let mut producers_by_type: HashMap<String, Vec<(String, i32)>> = HashMap::new();
    for node in node_map.values() {
        for ty in &node.produces {
            producers_by_type
                .entry(ty.clone())
                .or_default()
                .push((node.node_id.clone(), node.priority));
        }
    }

    let mut edges = Vec::new();
    let mut dependencies: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for (node_id, node) in &node_map {
        for dep in &node.control_deps {
            edges.push(DagEdge {
                from: dep.clone(),
                to: node_id.clone(),
                kind: DagEdgeKind::Control,
            });
            dependencies
                .entry(node_id.clone())
                .or_default()
                .insert(dep.clone());
        }

        for input in &node.consumes {
            let mut candidates = producers_by_type
                .get(&input.input_type)
                .cloned()
                .unwrap_or_default();
            candidates.retain(|(producer, _)| producer != node_id);

            let selected = if let Some(explicit) = &input.explicit_producer {
                if !candidates.iter().any(|(id, _)| id == explicit) {
                    return Err(DagBuildError::ExplicitProducerNotFound {
                        consumer: node_id.clone(),
                        input_type: input.input_type.clone(),
                        producer: explicit.clone(),
                    });
                }
                Some(explicit.clone())
            } else if candidates.is_empty() {
                if input.required {
                    return Err(DagBuildError::MissingRequiredInput {
                        consumer: node_id.clone(),
                        input_type: input.input_type.clone(),
                    });
                }
                None
            } else if candidates.len() > 1 && policy.require_explicit_binding_for_multi_producer {
                let mut ids: Vec<String> = candidates.into_iter().map(|(id, _)| id).collect();
                ids.sort();
                return Err(DagBuildError::ProducerConflict {
                    consumer: node_id.clone(),
                    input_type: input.input_type.clone(),
                    producers: ids,
                });
            } else {
                // Deterministic resolution: priority(desc) -> node_id(asc)
                candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                Some(candidates[0].0.clone())
            };

            if let Some(producer) = selected {
                edges.push(DagEdge {
                    from: producer.clone(),
                    to: node_id.clone(),
                    kind: DagEdgeKind::Data {
                        input_type: input.input_type.clone(),
                    },
                });
                dependencies
                    .entry(node_id.clone())
                    .or_default()
                    .insert(producer);
            }
        }
    }

    detect_cycle(&node_map, &edges)?;

    Ok(DagGraph {
        nodes: node_map,
        edges,
        dependencies,
    })
}

fn detect_cycle(
    nodes: &BTreeMap<String, DagNodeSpec>,
    edges: &[DagEdge],
) -> Result<(), DagBuildError> {
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    for edge in edges {
        adjacency
            .entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }

    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut stack = Vec::new();

    for node_id in nodes.keys() {
        if visited.contains(node_id) {
            continue;
        }
        if let Some(cycle_path) =
            dfs_cycle(node_id, &adjacency, &mut visiting, &mut visited, &mut stack)
        {
            return Err(DagBuildError::CycleDetected { cycle_path });
        }
    }

    Ok(())
}

fn dfs_cycle(
    node: &str,
    adjacency: &HashMap<String, Vec<String>>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    stack: &mut Vec<String>,
) -> Option<Vec<String>> {
    if visiting.contains(node) {
        if let Some(start) = stack.iter().position(|id| id == node) {
            let mut cycle = stack[start..].to_vec();
            cycle.push(node.to_string());
            return Some(cycle);
        }
    }
    if visited.contains(node) {
        return None;
    }

    visiting.insert(node.to_string());
    stack.push(node.to_string());

    if let Some(neighbors) = adjacency.get(node) {
        for next in neighbors {
            if let Some(cycle) = dfs_cycle(next, adjacency, visiting, visited, stack) {
                return Some(cycle);
            }
        }
    }

    stack.pop();
    visiting.remove(node);
    visited.insert(node.to_string());
    None
}
