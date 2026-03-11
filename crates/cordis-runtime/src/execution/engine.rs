//! Integrated execution engine.
//! It wires DAG build, deterministic scheduling, Gate aggregation, retries,
//! cancellation propagation, and Router overlay commit/rollback semantics.

use crate::context::{ContextWrite, RuntimeContext};
use crate::core::error::RuntimeError;
use crate::core::models::{GatePolicy, NodeOutcome};
use crate::execution::actor::{ActorCommand, ActorExecutor};
use crate::execution::dag::{build_dag, DagBuildPolicy, DagGraph, DagNodeSpec};
use crate::execution::gate::{evaluate_gate, BackoffPolicy, GateDecision, RunPolicy};
use crate::execution::router::{execute_router, RouterMetrics};
use crate::execution::scheduler::SchedulerConfig;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionNodeKind {
    Task,
    Gate { policy: GatePolicy },
    Router { subgraph_id: String },
    Terminal,
}

#[derive(Debug, Clone)]
pub struct ExecutionNodeSpec {
    pub dag: DagNodeSpec,
    pub run_policy: RunPolicy,
    pub kind: ExecutionNodeKind,
}

#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    pub scheduler: SchedulerConfig,
    pub dag_policy: DagBuildPolicy,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            scheduler: SchedulerConfig::conservative(),
            dag_policy: DagBuildPolicy::default(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ExecutionMetrics {
    pub dag_build_ms: u128,
    pub dag_cycle_detected_total: u64,
    pub node_retry_total: u64,
    pub gate_wait_ms: u128,
    pub execution_cancel_total: u64,
    pub router: RouterMetrics,
}

#[derive(Debug, Clone)]
pub struct ExecutionOutput {
    pub execution_id: String,
    /// Actual node execution attempts order (includes retries).
    pub order: Vec<String>,
    /// Final terminal outcomes for all nodes.
    pub outcomes: BTreeMap<String, NodeOutcome>,
    pub metrics: ExecutionMetrics,
}

#[derive(Debug, Clone)]
struct ReadyItem {
    node_id: String,
    topo_level: usize,
    priority: i32,
    retry: bool,
}

fn cmp_ready(a: &ReadyItem, b: &ReadyItem) -> Ordering {
    a.topo_level
        .cmp(&b.topo_level)
        .then_with(|| b.priority.cmp(&a.priority))
        .then_with(|| a.node_id.cmp(&b.node_id))
        .then_with(|| b.retry.cmp(&a.retry))
}

pub fn execute_graph<F>(
    config: ExecutionConfig,
    nodes: Vec<ExecutionNodeSpec>,
    context: &mut RuntimeContext,
    mut runner: F,
) -> Result<ExecutionOutput, RuntimeError>
where
    F: FnMut(&ExecutionNodeSpec, u32, &mut RuntimeContext) -> NodeOutcome,
{
    let execution_id = make_execution_id();
    let run_result = (|| {
        let mut metrics = ExecutionMetrics::default();

        let build_started = Instant::now();
        let dag_nodes = nodes.iter().map(|x| x.dag.clone()).collect::<Vec<_>>();
        let dag = match build_dag(dag_nodes, config.dag_policy) {
            Ok(graph) => graph,
            Err(err) => {
                return Err(RuntimeError::DagBuild {
                    message: err.to_string(),
                });
            }
        };
        metrics.dag_build_ms = build_started.elapsed().as_millis();

        let specs = nodes
            .into_iter()
            .map(|node| (node.dag.node_id.clone(), node))
            .collect::<BTreeMap<_, _>>();
        let deps = collect_dependencies(&dag);
        let dependents = collect_dependents(&dag);
        let topo_levels = compute_topo_levels(&dag, &deps, &dependents);
        let mut actor = ActorExecutor::new(config.scheduler.max_parallelism);

        let mut state = EngineState::new(specs, deps, dependents, topo_levels, metrics);

        for node_id in state.specs.keys().cloned().collect::<Vec<_>>() {
            state.process_candidate(&node_id, context)?;
        }
        state.process_terminal_queue(context)?;

        while !state.ready.is_empty() {
            let mut items = state.ready.drain(..).collect::<Vec<_>>();
            items.sort_by(cmp_ready);
            let batch_size = config
                .scheduler
                .max_parallelism
                .max(1)
                .min(items.len().max(1));
            let batch = items.drain(..batch_size).collect::<Vec<_>>();
            for item in items {
                state.ready.push_back(item);
            }

            for item in batch {
                state.ready_set.remove(&item.node_id);
                if state.outcomes.contains_key(&item.node_id) {
                    continue;
                }
                let attempt = *state.attempts.get(&item.node_id).unwrap_or(&0);
                actor.submit(ActorCommand::RunNode {
                    node_id: item.node_id,
                    attempt,
                });
            }

            let specs = &state.specs;
            let events = actor.dispatch_batch(|node_id, attempt| {
                let spec = specs.get(node_id).ok_or_else(|| RuntimeError::Invariant {
                    message: format!("ready node missing spec: {node_id}"),
                })?;
                run_node(spec, attempt, &mut state.metrics, context, &mut runner)
            });

            for event in events {
                let node_id = event.node_id.clone();
                state.order.push(node_id.clone());
                let outcome = event.result?;
                let spec = state
                    .specs
                    .get(&node_id)
                    .ok_or_else(|| RuntimeError::Invariant {
                        message: format!("event node missing spec: {node_id}"),
                    })?;

                match outcome {
                    NodeOutcome::Failure | NodeOutcome::Timeout => {
                        let next_attempt = event.attempt + 1;
                        if next_attempt <= spec.run_policy.max_retries {
                            state.attempts.insert(node_id.clone(), next_attempt);
                            state.metrics.node_retry_total += 1;
                            let delay_ms = retry_backoff_delay_ms(&spec.run_policy, next_attempt);
                            if delay_ms > 0 {
                                sleep(Duration::from_millis(delay_ms));
                            }
                            state.push_ready(&node_id, true)?;
                        } else {
                            state.set_terminal(&node_id, outcome, context)?;
                        }
                    }
                    NodeOutcome::Success
                    | NodeOutcome::Cancelled
                    | NodeOutcome::Skipped => {
                        state.set_terminal(&node_id, outcome, context)?;
                    }
                }
            }

            state.process_terminal_queue(context)?;
        }

        // Conservative close-out: unresolved nodes become Skipped.
        for node_id in state.specs.keys().cloned().collect::<Vec<_>>() {
            if !state.outcomes.contains_key(&node_id) {
                state.set_terminal(&node_id, NodeOutcome::Skipped, context)?;
            }
        }
        state.process_terminal_queue(context)?;

        Ok(ExecutionOutput {
            execution_id: execution_id.clone(),
            order: state.order,
            outcomes: state.outcomes,
            metrics: state.metrics,
        })
    })();

    run_result.map_err(|err| match err {
        RuntimeError::ExecutionFailed { .. } => err,
        _ => RuntimeError::ExecutionFailed {
            execution_id,
            message: err.to_string(),
        },
    })
}

fn run_node<F>(
    spec: &ExecutionNodeSpec,
    attempt: u32,
    metrics: &mut ExecutionMetrics,
    context: &mut RuntimeContext,
    runner: &mut F,
) -> Result<NodeOutcome, RuntimeError>
where
    F: FnMut(&ExecutionNodeSpec, u32, &mut RuntimeContext) -> NodeOutcome,
{
    let started_at = Instant::now();

    match &spec.kind {
        ExecutionNodeKind::Router { subgraph_id } => {
            let result = execute_router(context, subgraph_id, &mut metrics.router, |ctx| {
                runner(spec, attempt, ctx)
            }, spec.run_policy.timeout_ms)?;
            Ok(result.outcome)
        }
        ExecutionNodeKind::Task | ExecutionNodeKind::Gate { .. } | ExecutionNodeKind::Terminal => {
            let outcome = runner(spec, attempt, context);
            if spec.run_policy.timeout_ms > 0
                && started_at.elapsed() > Duration::from_millis(spec.run_policy.timeout_ms)
            {
                Ok(NodeOutcome::Timeout)
            } else {
                Ok(outcome)
            }
        }
    }
}

struct EngineState {
    specs: BTreeMap<String, ExecutionNodeSpec>,
    deps: BTreeMap<String, Vec<String>>,
    dependents: BTreeMap<String, Vec<String>>,
    topo_levels: BTreeMap<String, usize>,
    ready: VecDeque<ReadyItem>,
    ready_set: HashSet<String>,
    attempts: HashMap<String, u32>,
    order: Vec<String>,
    outcomes: BTreeMap<String, NodeOutcome>,
    completion_order: Vec<String>,
    terminal_queue: VecDeque<String>,
    gate_wait_started: HashMap<String, Instant>,
    metrics: ExecutionMetrics,
}

impl EngineState {
    fn new(
        specs: BTreeMap<String, ExecutionNodeSpec>,
        deps: BTreeMap<String, Vec<String>>,
        dependents: BTreeMap<String, Vec<String>>,
        topo_levels: BTreeMap<String, usize>,
        metrics: ExecutionMetrics,
    ) -> Self {
        Self {
            specs,
            deps,
            dependents,
            topo_levels,
            ready: VecDeque::new(),
            ready_set: HashSet::new(),
            attempts: HashMap::new(),
            order: Vec::new(),
            outcomes: BTreeMap::new(),
            completion_order: Vec::new(),
            terminal_queue: VecDeque::new(),
            gate_wait_started: HashMap::new(),
            metrics,
        }
    }

    fn push_ready(&mut self, node_id: &str, retry: bool) -> Result<(), RuntimeError> {
        if self.outcomes.contains_key(node_id) || self.ready_set.contains(node_id) {
            return Ok(());
        }
        let spec = self.specs.get(node_id).ok_or_else(|| RuntimeError::Invariant {
            message: format!("push_ready missing spec: {node_id}"),
        })?;
        let topo_level = *self
            .topo_levels
            .get(node_id)
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!("push_ready missing topo level: {node_id}"),
            })?;
        self.ready.push_back(ReadyItem {
            node_id: node_id.to_string(),
            topo_level,
            priority: spec.dag.priority,
            retry,
        });
        self.ready_set.insert(node_id.to_string());
        Ok(())
    }

    fn set_terminal(
        &mut self,
        node_id: &str,
        outcome: NodeOutcome,
        context: &mut RuntimeContext,
    ) -> Result<(), RuntimeError> {
        if self.outcomes.contains_key(node_id) {
            return Ok(());
        }

        self.outcomes.insert(node_id.to_string(), outcome);
        self.completion_order.push(node_id.to_string());
        self.terminal_queue.push_back(node_id.to_string());
        self.ready.retain(|item| item.node_id != node_id);
        self.ready_set.remove(node_id);

        if outcome == NodeOutcome::Cancelled {
            self.metrics.execution_cancel_total += 1;
        }
        if outcome == NodeOutcome::Skipped {
            context.mark_skipped(node_id)?;
        }

        Ok(())
    }

    fn finalize_gate_wait(&mut self, node_id: &str) {
        if let Some(started_at) = self.gate_wait_started.remove(node_id) {
            self.metrics.gate_wait_ms += started_at.elapsed().as_millis();
        }
    }

    fn process_candidate(
        &mut self,
        node_id: &str,
        context: &mut RuntimeContext,
    ) -> Result<(), RuntimeError> {
        if self.outcomes.contains_key(node_id) {
            return Ok(());
        }

        let spec = self.specs.get(node_id).ok_or_else(|| RuntimeError::Invariant {
            message: format!("process_candidate missing spec: {node_id}"),
        })?;
        let upstream = self.deps.get(node_id).cloned().unwrap_or_default();

        if let ExecutionNodeKind::Gate { policy } = &spec.kind {
            let decision = evaluate_gate(*policy, &upstream, &self.outcomes, &self.completion_order);
            match decision {
                GateDecision::Wait => {
                    if upstream.iter().any(|id| self.outcomes.contains_key(id)) {
                        self.gate_wait_started
                            .entry(node_id.to_string())
                            .or_insert_with(Instant::now);
                    }
                }
                GateDecision::CompleteSuccess => {
                    self.finalize_gate_wait(node_id);
                    self.set_terminal(node_id, NodeOutcome::Success, context)?;
                }
                GateDecision::CompleteFailure => {
                    self.finalize_gate_wait(node_id);
                    self.set_terminal(node_id, NodeOutcome::Failure, context)?;
                }
                GateDecision::CompleteAndCancel {
                    success,
                    cancel_nodes,
                } => {
                    self.finalize_gate_wait(node_id);
                    for cancel in cancel_nodes {
                        self.set_terminal(&cancel, NodeOutcome::Cancelled, context)?;
                    }
                    let gate_outcome = if success {
                        NodeOutcome::Success
                    } else {
                        NodeOutcome::Failure
                    };
                    self.set_terminal(node_id, gate_outcome, context)?;
                }
            }
            return Ok(());
        }

        let mut all_terminal = true;
        let mut all_success = true;

        for dep in &upstream {
            match self.outcomes.get(dep) {
                Some(NodeOutcome::Success) => {}
                Some(
                    NodeOutcome::Failure
                    | NodeOutcome::Timeout
                    | NodeOutcome::Cancelled
                    | NodeOutcome::Skipped,
                ) => {
                    all_success = false;
                }
                None => {
                    all_terminal = false;
                    all_success = false;
                }
            }
        }

        if !all_terminal {
            return Ok(());
        }

        if all_success {
            self.push_ready(node_id, false)?;
        } else {
            self.set_terminal(node_id, NodeOutcome::Skipped, context)?;
        }
        Ok(())
    }

    fn process_terminal_queue(&mut self, context: &mut RuntimeContext) -> Result<(), RuntimeError> {
        while let Some(done) = self.terminal_queue.pop_front() {
            let Some(children) = self.dependents.get(&done) else {
                continue;
            };
            for child in children.clone() {
                self.process_candidate(&child, context)?;
            }
        }
        Ok(())
    }
}

fn collect_dependencies(graph: &DagGraph) -> BTreeMap<String, Vec<String>> {
    graph
        .nodes
        .keys()
        .map(|id| {
            (
                id.clone(),
                graph
                    .dependencies
                    .get(id)
                    .map(|x| x.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

fn collect_dependents(graph: &DagGraph) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in &graph.edges {
        out.entry(edge.from.clone()).or_default().push(edge.to.clone());
    }
    for children in out.values_mut() {
        children.sort();
        children.dedup();
    }
    out
}

fn compute_topo_levels(
    graph: &DagGraph,
    deps: &BTreeMap<String, Vec<String>>,
    dependents: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, usize> {
    let mut levels = BTreeMap::new();
    let mut indegree = HashMap::new();
    for id in graph.nodes.keys() {
        indegree.insert(id.clone(), deps.get(id).map(|x| x.len()).unwrap_or(0));
    }

    let mut queue = graph
        .nodes
        .keys()
        .filter(|id| indegree.get(*id).copied().unwrap_or(0) == 0)
        .cloned()
        .collect::<BTreeSet<_>>();

    while let Some(node_id) = queue.pop_first() {
        let level = deps
            .get(&node_id)
            .map(|x| {
                x.iter()
                    .map(|dep| levels.get(dep).copied().unwrap_or(0) + 1)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        levels.insert(node_id.clone(), level);

        if let Some(children) = dependents.get(&node_id) {
            for child in children {
                if let Some(in_count) = indegree.get_mut(child) {
                    if *in_count > 0 {
                        *in_count -= 1;
                    }
                    if *in_count == 0 {
                        queue.insert(child.clone());
                    }
                }
            }
        }
    }

    for id in graph.nodes.keys() {
        levels.entry(id.clone()).or_insert(0);
    }
    levels
}

fn make_execution_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("exec-{nanos}")
}

fn retry_backoff_delay_ms(policy: &RunPolicy, next_attempt: u32) -> u64 {
    match &policy.backoff {
        BackoffPolicy::None => 0,
        BackoffPolicy::Fixed { delay_ms } => *delay_ms,
        BackoffPolicy::Exponential { base_ms, max_ms } => {
            if next_attempt == 0 {
                return 0;
            }
            let shift = next_attempt.saturating_sub(1).min(20);
            let factor = 1u64 << shift;
            base_ms.saturating_mul(factor).min(*max_ms)
        }
    }
}
