//! Colored Petri Net execution engine.
//! It wires net build, keyed matching/join policies, retries/backoff,
//! router overlay semantics, and strict late-token tombstoning.

use crate::context::{ContextWrite, RuntimeContext};
use crate::core::error::RuntimeError;
use crate::core::models::NodeOutcome;
use crate::execution::gate::{BackoffPolicy, RunPolicy};
use crate::execution::net::{
    build_petri_net, ArcDirection, ArcSpec, CorrelationKey, JoinPolicy, PetriNetBuildError,
    PetriNetGraph, PetriNetSpec, PlaceSpec, Token, TokenMeta, TransitionSpec,
};
use crate::execution::router::{execute_router, RouterMetrics};
use crate::execution::scheduler::SchedulerConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerMode {
    Throughput,
    Deterministic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionTransitionKind {
    Task,
    Router { subgraph_id: String },
    Terminal,
}

#[derive(Debug, Clone)]
pub struct ExecutionTransitionSpec {
    pub transition: TransitionSpec,
    pub run_policy: RunPolicy,
    pub kind: ExecutionTransitionKind,
    pub logical_group: Option<String>,
    /// Topological level from the registered net; lower levels execute first.
    pub topo_level: usize,
}

#[derive(Debug, Clone)]
pub struct ExecutionNetSpec {
    pub places: Vec<PlaceSpec>,
    pub transitions: Vec<ExecutionTransitionSpec>,
    pub arcs: Vec<ArcSpec>,
}

#[derive(Debug, Clone)]
pub struct TriggerInput {
    pub place_id: String,
    pub label: Option<String>,
    pub token: Token,
}

#[derive(Debug, Clone)]
pub struct TransitionTrigger {
    pub key: CorrelationKey,
    pub inputs: Vec<TriggerInput>,
}

#[derive(Debug, Clone)]
pub struct TransitionRunResult {
    pub outcome: NodeOutcome,
    pub payload: Value,
}

impl TransitionRunResult {
    pub fn from_outcome(outcome: NodeOutcome) -> Self {
        Self {
            outcome,
            payload: Value::Null,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    pub scheduler: SchedulerConfig,
    pub scheduler_mode: SchedulerMode,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            scheduler: SchedulerConfig::conservative(),
            scheduler_mode: SchedulerMode::Throughput,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionMetrics {
    pub net_build_ms: u128,
    pub net_build_failed_total: u64,
    pub node_retry_total: u64,
    pub join_wait_ms: u128,
    pub execution_cancel_total: u64,
    pub late_token_total: u64,
    pub zombie_token_total: u64,
    pub router: RouterMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionOutput {
    pub execution_id: String,
    /// Actual transition execution order (includes retries).
    pub order: Vec<String>,
    /// Final outcomes by transition id (last observed per transition).
    pub outcomes: BTreeMap<String, NodeOutcome>,
    /// Final outcomes by transition id and correlation key.
    pub keyed_outcomes: BTreeMap<String, BTreeMap<String, NodeOutcome>>,
    pub metrics: ExecutionMetrics,
}

#[derive(Debug, Clone)]
struct ReadyItem {
    transition_id: String,
    key: CorrelationKey,
    topo_level: usize,
    priority: i32,
    retry: bool,
}

fn cmp_ready(a: &ReadyItem, b: &ReadyItem) -> Ordering {
    a.topo_level
        .cmp(&b.topo_level)
        .then_with(|| b.priority.cmp(&a.priority))
        .then_with(|| a.transition_id.cmp(&b.transition_id))
        .then_with(|| a.key.cmp(&b.key))
        .then_with(|| b.retry.cmp(&a.retry))
}

pub fn execute_net<F>(
    config: ExecutionConfig,
    net: ExecutionNetSpec,
    context: &mut RuntimeContext,
    mut runner: F,
) -> Result<ExecutionOutput, RuntimeError>
where
    F: FnMut(
        &ExecutionTransitionSpec,
        u32,
        &TransitionTrigger,
        &mut RuntimeContext,
    ) -> TransitionRunResult,
{
    let execution_id = make_execution_id();
    let run_result = (|| {
        let mut metrics = ExecutionMetrics::default();

        let build_started = Instant::now();
        let petri = PetriNetSpec {
            places: net.places,
            transitions: net
                .transitions
                .iter()
                .map(|x| x.transition.clone())
                .collect(),
            arcs: net.arcs,
        };
        let graph = match build_petri_net(petri) {
            Ok(graph) => graph,
            Err(err) => {
                return Err(map_build_error(err));
            }
        };
        metrics.net_build_ms = build_started.elapsed().as_millis();

        let specs = net
            .transitions
            .into_iter()
            .map(|spec| (spec.transition.transition_id.clone(), spec))
            .collect::<BTreeMap<_, _>>();

        let mut state = EngineState::new(execution_id.clone(), specs, graph, metrics);
        state.enqueue_roots()?;

        while !state.ready.is_empty() {
            let batch = state.next_batch(&config);
            for item in batch {
                let ready_key = (item.transition_id.clone(), item.key.clone());
                state.ready_set.remove(&ready_key);

                if state.is_key_done(&item.transition_id, &item.key) {
                    continue;
                }

                let attempt = *state
                    .attempts
                    .get(&(item.transition_id.clone(), item.key.clone()))
                    .unwrap_or(&0);

                let trigger = state.ensure_trigger_inputs(&item.transition_id, &item.key)?;
                let spec = state.specs.get(&item.transition_id).ok_or_else(|| {
                    RuntimeError::Invariant {
                        message: format!("ready transition missing spec: {}", item.transition_id),
                    }
                })?;

                state.order.push(item.transition_id.clone());
                let run = if spec.transition.join_policy == JoinPolicy::AllOf
                    && trigger
                        .inputs
                        .iter()
                        .any(|input| input.token.meta.outcome != NodeOutcome::Success)
                {
                    TransitionRunResult::from_outcome(NodeOutcome::Skipped)
                } else {
                    run_transition(
                        spec,
                        attempt,
                        &trigger,
                        &mut state.metrics,
                        context,
                        &mut runner,
                    )?
                };
                let outcome = run.outcome;

                match outcome {
                    NodeOutcome::Failure | NodeOutcome::Timeout => {
                        let next_attempt = attempt + 1;
                        if next_attempt <= spec.run_policy.max_retries {
                            state.attempts.insert(
                                (item.transition_id.clone(), item.key.clone()),
                                next_attempt,
                            );
                            state.metrics.node_retry_total += 1;
                            let delay_ms = retry_backoff_delay_ms(&spec.run_policy, next_attempt);
                            if delay_ms > 0 {
                                sleep(Duration::from_millis(delay_ms));
                            }
                            state.push_ready(&item.transition_id, item.key, true)?;
                            continue;
                        }
                    }
                    NodeOutcome::Success | NodeOutcome::Cancelled | NodeOutcome::Skipped => {}
                }

                state.complete_transition(&item.transition_id, &item.key, run, context)?;
            }
        }

        Ok(ExecutionOutput {
            execution_id: execution_id.clone(),
            order: state.order,
            outcomes: state.outcomes,
            keyed_outcomes: state.keyed_outcomes,
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

fn map_build_error(err: PetriNetBuildError) -> RuntimeError {
    RuntimeError::NetBuild {
        message: err.to_string(),
    }
}

fn run_transition<F>(
    spec: &ExecutionTransitionSpec,
    attempt: u32,
    trigger: &TransitionTrigger,
    metrics: &mut ExecutionMetrics,
    context: &mut RuntimeContext,
    runner: &mut F,
) -> Result<TransitionRunResult, RuntimeError>
where
    F: FnMut(
        &ExecutionTransitionSpec,
        u32,
        &TransitionTrigger,
        &mut RuntimeContext,
    ) -> TransitionRunResult,
{
    let started_at = Instant::now();

    match &spec.kind {
        ExecutionTransitionKind::Router { subgraph_id } => {
            let mut payload = Value::Null;
            let result = execute_router(
                context,
                subgraph_id,
                &mut metrics.router,
                |ctx| {
                    let run = runner(spec, attempt, trigger, ctx);
                    payload = run.payload;
                    run.outcome
                },
                spec.run_policy.timeout_ms,
            )?;
            Ok(TransitionRunResult {
                outcome: result.outcome,
                payload,
            })
        }
        ExecutionTransitionKind::Task | ExecutionTransitionKind::Terminal => {
            let mut result = runner(spec, attempt, trigger, context);
            if spec.run_policy.timeout_ms > 0
                && started_at.elapsed() > Duration::from_millis(spec.run_policy.timeout_ms)
            {
                result.outcome = NodeOutcome::Timeout;
            }
            Ok(result)
        }
    }
}

struct EngineState {
    execution_id: String,
    specs: BTreeMap<String, ExecutionTransitionSpec>,
    graph: PetriNetGraph,
    place_tokens: BTreeMap<String, BTreeMap<CorrelationKey, VecDeque<Token>>>,
    trigger_inputs_cache: HashMap<(String, CorrelationKey), Vec<TriggerInput>>,
    ready: VecDeque<ReadyItem>,
    ready_set: HashSet<(String, CorrelationKey)>,
    attempts: HashMap<(String, CorrelationKey), u32>,
    order: Vec<String>,
    outcomes: BTreeMap<String, NodeOutcome>,
    keyed_outcomes: BTreeMap<String, BTreeMap<String, NodeOutcome>>,
    completed_keys: BTreeMap<String, BTreeSet<CorrelationKey>>,
    join_wait_started: BTreeMap<(String, CorrelationKey), Instant>,
    token_sequence: u64,
    metrics: ExecutionMetrics,
}

impl EngineState {
    fn new(
        execution_id: String,
        specs: BTreeMap<String, ExecutionTransitionSpec>,
        graph: PetriNetGraph,
        metrics: ExecutionMetrics,
    ) -> Self {
        let place_tokens = graph
            .places
            .keys()
            .cloned()
            .map(|place_id| (place_id, BTreeMap::<CorrelationKey, VecDeque<Token>>::new()))
            .collect();
        Self {
            execution_id,
            specs,
            graph,
            place_tokens,
            trigger_inputs_cache: HashMap::new(),
            ready: VecDeque::new(),
            ready_set: HashSet::new(),
            attempts: HashMap::new(),
            order: Vec::new(),
            outcomes: BTreeMap::new(),
            keyed_outcomes: BTreeMap::new(),
            completed_keys: BTreeMap::new(),
            join_wait_started: BTreeMap::new(),
            token_sequence: 0,
            metrics,
        }
    }

    fn enqueue_roots(&mut self) -> Result<(), RuntimeError> {
        let transition_ids = self.specs.keys().cloned().collect::<Vec<_>>();
        for transition_id in transition_ids {
            let has_inputs = self
                .graph
                .input_arcs_by_transition
                .get(&transition_id)
                .map(|x| !x.is_empty())
                .unwrap_or(false);
            if !has_inputs {
                let key = if let Some(group) = self
                    .specs
                    .get(&transition_id)
                    .and_then(|spec| spec.logical_group.clone())
                {
                    CorrelationKey::new(format!("{}:group:{group}", self.execution_id))
                } else {
                    CorrelationKey::derive(&self.execution_id, &transition_id, "root")
                };
                self.push_ready(&transition_id, key, false)?;
            }
        }
        Ok(())
    }

    fn next_batch(&mut self, config: &ExecutionConfig) -> Vec<ReadyItem> {
        let batch_size = config
            .scheduler
            .max_parallelism
            .max(1)
            .min(self.ready.len().max(1));

        match config.scheduler_mode {
            SchedulerMode::Throughput => {
                let mut batch = Vec::new();
                for _ in 0..batch_size {
                    if let Some(item) = self.ready.pop_front() {
                        batch.push(item);
                    }
                }
                batch
            }
            SchedulerMode::Deterministic => {
                let mut items = self.ready.drain(..).collect::<Vec<_>>();
                items.sort_by(cmp_ready);
                let mut batch = Vec::new();
                for _ in 0..batch_size {
                    if items.is_empty() {
                        break;
                    }
                    batch.push(items.remove(0));
                }
                for item in items {
                    self.ready.push_back(item);
                }
                batch
            }
        }
    }

    fn push_ready(
        &mut self,
        transition_id: &str,
        key: CorrelationKey,
        retry: bool,
    ) -> Result<(), RuntimeError> {
        let ready_key = (transition_id.to_string(), key.clone());
        if self.ready_set.contains(&ready_key) || self.is_key_done(transition_id, &key) {
            return Ok(());
        }
        let spec = self
            .specs
            .get(transition_id)
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!("push_ready missing transition: {transition_id}"),
            })?;
        self.ready.push_back(ReadyItem {
            transition_id: transition_id.to_string(),
            key,
            topo_level: spec.topo_level,
            priority: spec.transition.priority,
            retry,
        });
        self.ready_set.insert(ready_key);
        Ok(())
    }

    fn is_key_done(&self, transition_id: &str, key: &CorrelationKey) -> bool {
        self.completed_keys
            .get(transition_id)
            .map(|set| set.contains(key))
            .unwrap_or(false)
    }

    fn ensure_trigger_inputs(
        &mut self,
        transition_id: &str,
        key: &CorrelationKey,
    ) -> Result<TransitionTrigger, RuntimeError> {
        let cache_key = (transition_id.to_string(), key.clone());
        if let Some(inputs) = self.trigger_inputs_cache.get(&cache_key) {
            return Ok(TransitionTrigger {
                key: key.clone(),
                inputs: inputs.clone(),
            });
        }

        if !self.is_transition_ready(transition_id, key)? {
            return Err(RuntimeError::Invariant {
                message: format!(
                    "transition became not-ready before run: transition={transition_id}, key={}",
                    key.0
                ),
            });
        }

        let inputs = self.consume_inputs_for_key(transition_id, key)?;
        self.trigger_inputs_cache.insert(cache_key, inputs.clone());
        self.finalize_join_wait(transition_id, key);

        Ok(TransitionTrigger {
            key: key.clone(),
            inputs,
        })
    }

    fn is_transition_ready(
        &mut self,
        transition_id: &str,
        key: &CorrelationKey,
    ) -> Result<bool, RuntimeError> {
        if self.is_key_done(transition_id, key) {
            return Ok(false);
        }

        let spec = self
            .specs
            .get(transition_id)
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!("missing transition spec: {transition_id}"),
            })?;
        let input_arcs = self
            .graph
            .input_arcs_by_transition
            .get(transition_id)
            .cloned()
            .unwrap_or_default();

        if input_arcs.is_empty() {
            return Ok(true);
        }

        let mut tokens_per_place = Vec::<(String, Vec<Token>)>::new();
        for arc in &input_arcs {
            let tokens = self
                .place_tokens
                .get(&arc.place_id)
                .and_then(|by_key| by_key.get(key))
                .map(|queue| queue.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            tokens_per_place.push((arc.place_id.clone(), tokens));
        }

        let ready = evaluate_join_policy(spec.transition.join_policy, &tokens_per_place);
        if !ready {
            if tokens_per_place
                .iter()
                .any(|(_, values)| !values.is_empty())
            {
                self.join_wait_started
                    .entry((transition_id.to_string(), key.clone()))
                    .or_insert_with(Instant::now);
            }
        }
        Ok(ready)
    }

    fn consume_inputs_for_key(
        &mut self,
        transition_id: &str,
        key: &CorrelationKey,
    ) -> Result<Vec<TriggerInput>, RuntimeError> {
        let input_arcs = self
            .graph
            .input_arcs_by_transition
            .get(transition_id)
            .cloned()
            .unwrap_or_default();

        let mut all_inputs = Vec::<TriggerInput>::new();
        for arc in input_arcs {
            let Some(by_key) = self.place_tokens.get_mut(&arc.place_id) else {
                return Err(RuntimeError::Invariant {
                    message: format!("missing place token bucket: {}", arc.place_id),
                });
            };
            let tokens = by_key.remove(key).unwrap_or_default();
            for token in tokens {
                all_inputs.push(TriggerInput {
                    place_id: arc.place_id.clone(),
                    label: arc.label.clone(),
                    token,
                });
            }
        }

        Ok(all_inputs)
    }

    fn complete_transition(
        &mut self,
        transition_id: &str,
        key: &CorrelationKey,
        run: TransitionRunResult,
        context: &mut RuntimeContext,
    ) -> Result<(), RuntimeError> {
        self.mark_completed(transition_id, key, run.outcome);
        self.trigger_inputs_cache
            .remove(&(transition_id.to_string(), key.clone()));
        if run.outcome == NodeOutcome::Skipped {
            context.mark_skipped(transition_id)?;
        }

        let output_arcs = self
            .graph
            .output_arcs_by_transition
            .get(transition_id)
            .cloned()
            .unwrap_or_default();
        let logical_group = self
            .specs
            .get(transition_id)
            .and_then(|spec| spec.logical_group.clone())
            .unwrap_or_else(|| "default".to_string());

        for arc in output_arcs {
            if arc.direction != ArcDirection::TransitionToPlace {
                continue;
            }
            let sequence = self.next_token_sequence();
            self.insert_token(
                &arc.place_id,
                Token {
                    key: key.clone(),
                    payload: run.payload.clone(),
                    meta: TokenMeta {
                        execution_id: self.execution_id.clone(),
                        transition_id: transition_id.to_string(),
                        logical_group: logical_group.clone(),
                        sequence,
                        outcome: run.outcome,
                    },
                },
            )?;
        }

        Ok(())
    }

    fn mark_completed(&mut self, transition_id: &str, key: &CorrelationKey, outcome: NodeOutcome) {
        self.completed_keys
            .entry(transition_id.to_string())
            .or_default()
            .insert(key.clone());
        self.finalize_join_wait(transition_id, key);

        self.outcomes.insert(transition_id.to_string(), outcome);
        self.keyed_outcomes
            .entry(transition_id.to_string())
            .or_default()
            .insert(key.0.clone(), outcome);

        if outcome == NodeOutcome::Cancelled {
            self.metrics.execution_cancel_total += 1;
        }
    }

    fn insert_token(&mut self, place_id: &str, token: Token) -> Result<(), RuntimeError> {
        if let Some(consumer_transition) = self.graph.consumer_by_place.get(place_id).cloned() {
            if self.is_key_done(&consumer_transition, &token.key) {
                self.metrics.late_token_total += 1;
                self.metrics.zombie_token_total += 1;
                return Ok(());
            }
        }

        let Some(by_key) = self.place_tokens.get_mut(place_id) else {
            return Err(RuntimeError::Invariant {
                message: format!("insert token missing place: {place_id}"),
            });
        };
        by_key
            .entry(token.key.clone())
            .or_default()
            .push_back(token.clone());

        if let Some(consumer_transition) = self.graph.consumer_by_place.get(place_id).cloned() {
            if self.is_transition_ready(&consumer_transition, &token.key)? {
                self.push_ready(&consumer_transition, token.key.clone(), false)?;
            }
        }

        Ok(())
    }

    fn finalize_join_wait(&mut self, transition_id: &str, key: &CorrelationKey) {
        if let Some(started_at) = self
            .join_wait_started
            .remove(&(transition_id.to_string(), key.clone()))
        {
            self.metrics.join_wait_ms += started_at.elapsed().as_millis();
        }
    }

    fn next_token_sequence(&mut self) -> u64 {
        self.token_sequence += 1;
        self.token_sequence
    }
}

fn evaluate_join_policy(policy: JoinPolicy, tokens_per_place: &[(String, Vec<Token>)]) -> bool {
    match policy {
        JoinPolicy::AllOf | JoinPolicy::KeyedPair => {
            !tokens_per_place.is_empty() && tokens_per_place.iter().all(|(_, t)| !t.is_empty())
        }
        JoinPolicy::AnyOf => tokens_per_place.iter().any(|(_, t)| !t.is_empty()),
        JoinPolicy::Quorum(k) => {
            if k == 0 {
                return true;
            }
            tokens_per_place.iter().map(|(_, t)| t.len()).sum::<usize>() >= k
        }
        JoinPolicy::FirstSuccess => {
            let has_success = tokens_per_place
                .iter()
                .flat_map(|(_, t)| t.iter())
                .any(|token| token.meta.outcome == NodeOutcome::Success);
            if has_success {
                return true;
            }
            // If every place has at least one terminal token and no success exists,
            // we can still release as failure branch completion.
            !tokens_per_place.is_empty() && tokens_per_place.iter().all(|(_, t)| !t.is_empty())
        }
        JoinPolicy::FirstCompleted => tokens_per_place.iter().any(|(_, t)| !t.is_empty()),
        JoinPolicy::KeyedGroup => tokens_per_place.iter().any(|(_, t)| !t.is_empty()),
    }
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
