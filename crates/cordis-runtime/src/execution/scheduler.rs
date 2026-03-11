use crate::core::models::NodeOutcome;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};

#[derive(Debug, Clone)]
pub struct ScheduledNode {
    pub id: String,
    pub topo_level: usize,
    pub priority: i32,
    pub deps: Vec<String>,
    pub max_retries: u32,
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub max_parallelism: usize,
}

#[derive(Debug, Clone)]
struct ReadyItem {
    node_id: String,
    topo_level: usize,
    priority: i32,
    retry: bool,
}

#[derive(Debug, Clone)]
pub struct ExecutionReport {
    pub order: Vec<String>,
    pub outcomes: BTreeMap<String, NodeOutcome>,
}

impl SchedulerConfig {
    pub fn conservative() -> Self {
        Self { max_parallelism: 1 }
    }
}

fn cmp_ready(a: &ReadyItem, b: &ReadyItem) -> Ordering {
    a.topo_level
        .cmp(&b.topo_level)
        .then_with(|| b.priority.cmp(&a.priority))
        .then_with(|| a.node_id.cmp(&b.node_id))
        .then_with(|| b.retry.cmp(&a.retry))
}

pub fn run_deterministic<F>(
    config: SchedulerConfig,
    nodes: Vec<ScheduledNode>,
    mut runner: F,
) -> ExecutionReport
where
    F: FnMut(&ScheduledNode, u32) -> NodeOutcome,
{
    let node_map: HashMap<String, ScheduledNode> = nodes
        .into_iter()
        .map(|n| (n.id.clone(), n))
        .collect();

    let mut dep_count: HashMap<String, usize> = HashMap::new();
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();

    for node in node_map.values() {
        dep_count.insert(node.id.clone(), node.deps.len());
        for dep in &node.deps {
            dependents
                .entry(dep.clone())
                .or_default()
                .push(node.id.clone());
        }
    }

    let mut ready = VecDeque::new();
    for node in node_map.values() {
        if dep_count.get(&node.id).copied().unwrap_or(0) == 0 {
            ready.push_back(ReadyItem {
                node_id: node.id.clone(),
                topo_level: node.topo_level,
                priority: node.priority,
                retry: false,
            });
        }
    }

    let mut attempts: HashMap<String, u32> = HashMap::new();
    let mut order = Vec::new();
    let mut outcomes = BTreeMap::new();

    while !ready.is_empty() {
        let mut items: Vec<_> = ready.drain(..).collect();
        items.sort_by(cmp_ready);

        let batch = items
            .drain(..config.max_parallelism.max(1).min(items.len().max(1)))
            .collect::<Vec<_>>();

        for item in items {
            ready.push_back(item);
        }

        for item in batch {
            let node = node_map
                .get(&item.node_id)
                .expect("ready queue must reference an existing node");
            let attempt = *attempts.get(&node.id).unwrap_or(&0);
            let outcome = runner(node, attempt);
            order.push(node.id.clone());

            match outcome {
                NodeOutcome::Success => {
                    outcomes.insert(node.id.clone(), NodeOutcome::Success);
                    if let Some(children) = dependents.get(&node.id) {
                        for child in children {
                            if let Some(counter) = dep_count.get_mut(child) {
                                if *counter > 0 {
                                    *counter -= 1;
                                    if *counter == 0 {
                                        let child_node = node_map.get(child).expect("child node exists");
                                        ready.push_back(ReadyItem {
                                            node_id: child_node.id.clone(),
                                            topo_level: child_node.topo_level,
                                            priority: child_node.priority,
                                            retry: false,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                NodeOutcome::Failure | NodeOutcome::Timeout => {
                    let next_attempt = attempt + 1;
                    if next_attempt <= node.max_retries {
                        attempts.insert(node.id.clone(), next_attempt);
                        ready.push_back(ReadyItem {
                            node_id: node.id.clone(),
                            topo_level: node.topo_level,
                            priority: node.priority,
                            retry: true,
                        });
                    } else {
                        outcomes.insert(node.id.clone(), outcome);
                    }
                }
                NodeOutcome::Cancelled | NodeOutcome::Skipped => {
                    outcomes.insert(node.id.clone(), outcome);
                }
            }
        }
    }

    ExecutionReport { order, outcomes }
}
