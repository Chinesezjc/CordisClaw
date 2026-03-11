use cordis_runtime::context::{
    ContextKey, ContextRead, ContextTxn, ContextWrite, RuntimeContext, Sensitivity, SlotMeta,
};
use cordis_runtime::execution::dag::{
    build_dag, DagBuildError, DagBuildPolicy, DagInputSpec, DagNodeSpec,
};
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::{GatePolicy, NodeOutcome};
use cordis_runtime::execution::engine::{
    execute_graph, ExecutionConfig, ExecutionNodeKind, ExecutionNodeSpec,
};
use cordis_runtime::execution::gate::{evaluate_gate, BackoffPolicy, GateDecision, RunPolicy};
use cordis_runtime::execution::scheduler::SchedulerConfig;
use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::{Duration, Instant};

fn node(id: &str, priority: i32, produces: &[&str], consumes: Vec<DagInputSpec>) -> DagNodeSpec {
    DagNodeSpec {
        node_id: id.to_string(),
        priority,
        consumes,
        produces: produces.iter().map(|x| x.to_string()).collect(),
        control_deps: Vec::new(),
    }
}

fn exec_node(
    id: &str,
    priority: i32,
    control_deps: &[&str],
    max_retries: u32,
    kind: ExecutionNodeKind,
) -> ExecutionNodeSpec {
    ExecutionNodeSpec {
        dag: DagNodeSpec {
            node_id: id.to_string(),
            priority,
            consumes: Vec::new(),
            produces: Vec::new(),
            control_deps: control_deps.iter().map(|x| x.to_string()).collect(),
        },
        run_policy: RunPolicy {
            max_retries,
            ..RunPolicy::default()
        },
        kind,
    }
}

fn exec_node_with_policy(
    id: &str,
    priority: i32,
    control_deps: &[&str],
    run_policy: RunPolicy,
    kind: ExecutionNodeKind,
) -> ExecutionNodeSpec {
    ExecutionNodeSpec {
        dag: DagNodeSpec {
            node_id: id.to_string(),
            priority,
            consumes: Vec::new(),
            produces: Vec::new(),
            control_deps: control_deps.iter().map(|x| x.to_string()).collect(),
        },
        run_policy,
        kind,
    }
}

#[test]
fn dag_fails_on_multi_producer_without_explicit_binding() {
    let nodes = vec![
        node("a", 1, &["X"], vec![]),
        node("b", 2, &["X"], vec![]),
        node(
            "c",
            1,
            &[],
            vec![DagInputSpec {
                input_type: "X".to_string(),
                required: true,
                explicit_producer: None,
            }],
        ),
    ];

    let err = build_dag(nodes, DagBuildPolicy::default()).expect_err("must fail");
    assert!(matches!(err, DagBuildError::ProducerConflict { .. }));
}

#[test]
fn dag_cycle_detection_returns_full_path() {
    let mut a = node("a", 1, &["A"], vec![]);
    a.control_deps = vec!["b".to_string()];
    let mut b = node("b", 1, &["B"], vec![]);
    b.control_deps = vec!["a".to_string()];

    let err = build_dag(vec![a, b], DagBuildPolicy::default()).expect_err("must fail");
    match err {
        DagBuildError::CycleDetected { cycle_path } => {
            assert!(cycle_path.len() >= 3);
            assert_eq!(cycle_path.first().unwrap(), cycle_path.last().unwrap());
            assert!(cycle_path.contains(&"a".to_string()));
            assert!(cycle_path.contains(&"b".to_string()));
        }
        _ => panic!("expected cycle error"),
    }
}

#[test]
fn gate_first_success_cancels_other_pending_branches() {
    let upstream = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let mut outcomes = BTreeMap::new();
    outcomes.insert("a".to_string(), NodeOutcome::Failure);
    outcomes.insert("b".to_string(), NodeOutcome::Success);
    let completion_order = vec!["a".to_string(), "b".to_string()];

    let decision = evaluate_gate(
        GatePolicy::FirstSuccess,
        &upstream,
        &outcomes,
        &completion_order,
    );

    match decision {
        GateDecision::CompleteAndCancel { cancel_nodes, .. } => {
            assert_eq!(cancel_nodes, vec!["c".to_string()]);
        }
        _ => panic!("unexpected gate decision: {decision:?}"),
    }
}

#[test]
fn context_txn_commit_and_rollback_work() {
    let mut ctx = RuntimeContext::default();

    let base_key = ContextKey {
        namespace: "ns".to_string(),
        name: "base".to_string(),
        version: 100,
    };
    let staged_key = ContextKey {
        namespace: "ns".to_string(),
        name: "staged".to_string(),
        version: 100,
    };

    let meta = SlotMeta {
        required: true,
        ttl_ms: None,
        sensitivity: Sensitivity::Internal,
        owner: "test".to_string(),
    };

    ctx.put(base_key.clone(), 1_u64, meta.clone()).unwrap();
    assert_eq!(ctx.get::<u64>(&base_key).unwrap(), Some(1));

    ctx.begin_subgraph("sg1").unwrap();
    ctx.put(staged_key.clone(), 2_u64, meta.clone()).unwrap();
    assert_eq!(ctx.get::<u64>(&staged_key).unwrap(), Some(2));
    ctx.rollback_overlay("sg1").unwrap();
    assert_eq!(ctx.get::<u64>(&staged_key).unwrap(), None);

    ctx.begin_subgraph("sg2").unwrap();
    ctx.put(staged_key.clone(), 3_u64, meta).unwrap();
    ctx.commit_overlay("sg2").unwrap();
    assert_eq!(ctx.get::<u64>(&staged_key).unwrap(), Some(3));
}

#[test]
fn context_session_commit_uses_cas() {
    let mut ctx = RuntimeContext::default();
    let key = ContextKey {
        namespace: "session".to_string(),
        name: "counter".to_string(),
        version: 100,
    };
    let meta = SlotMeta {
        required: false,
        ttl_ms: None,
        sensitivity: Sensitivity::Low,
        owner: "test".to_string(),
    };

    ctx.put(key, 42_u64, meta).unwrap();
    ctx.commit_session("s1", 0).expect("first commit should pass");
    assert_eq!(ctx.session_version(), 1);

    let err = ctx.commit_session("s1", 0).expect_err("must fail by CAS");
    assert!(matches!(err, RuntimeError::CommitConflict { .. }));

    let metrics = ctx.metrics();
    assert!(metrics.context_write_total >= 1);
    assert_eq!(metrics.session_commit_conflict_total, 1);
}

#[test]
fn engine_first_success_cancels_pending_branch() {
    let nodes = vec![
        exec_node("mirror_a", 10, &[], 0, ExecutionNodeKind::Task),
        exec_node("mirror_b", 1, &[], 0, ExecutionNodeKind::Task),
        exec_node(
            "gate",
            1,
            &["mirror_a", "mirror_b"],
            0,
            ExecutionNodeKind::Gate {
                policy: GatePolicy::FirstSuccess,
            },
        ),
        exec_node("terminal", 1, &["gate"], 0, ExecutionNodeKind::Terminal),
    ];

    let mut ctx = RuntimeContext::default();
    let output = execute_graph(
        ExecutionConfig {
            scheduler: SchedulerConfig { max_parallelism: 1 },
            ..ExecutionConfig::default()
        },
        nodes,
        &mut ctx,
        |spec, _, _| match spec.dag.node_id.as_str() {
            "mirror_a" => NodeOutcome::Success,
            "mirror_b" => NodeOutcome::Success,
            "terminal" => NodeOutcome::Success,
            _ => NodeOutcome::Success,
        },
    )
    .expect("engine run should pass");

    assert_eq!(output.outcomes.get("mirror_a"), Some(&NodeOutcome::Success));
    assert_eq!(output.outcomes.get("mirror_b"), Some(&NodeOutcome::Cancelled));
    assert_eq!(output.outcomes.get("gate"), Some(&NodeOutcome::Success));
    assert_eq!(output.outcomes.get("terminal"), Some(&NodeOutcome::Success));
    assert_eq!(output.metrics.execution_cancel_total, 1);
    assert_eq!(output.order, vec!["mirror_a", "terminal"]);
}

#[test]
fn engine_router_failure_rolls_back_overlay() {
    let nodes = vec![
        exec_node(
            "router",
            1,
            &[],
            0,
            ExecutionNodeKind::Router {
                subgraph_id: "sg1".to_string(),
            },
        ),
        exec_node("after_router", 1, &["router"], 0, ExecutionNodeKind::Terminal),
    ];

    let mut ctx = RuntimeContext::default();
    let key = ContextKey {
        namespace: "router".to_string(),
        name: "result".to_string(),
        version: 100,
    };
    let meta = SlotMeta {
        required: false,
        ttl_ms: None,
        sensitivity: Sensitivity::Internal,
        owner: "test".to_string(),
    };

    let output = execute_graph(
        ExecutionConfig::default(),
        nodes,
        &mut ctx,
        |spec, _, ctx| {
            if spec.dag.node_id == "router" {
                ctx.put(key.clone(), 42_u64, meta.clone())
                    .expect("write to overlay");
                NodeOutcome::Failure
            } else {
                NodeOutcome::Success
            }
        },
    )
    .expect("engine run should pass");

    assert_eq!(output.outcomes.get("router"), Some(&NodeOutcome::Failure));
    assert_eq!(
        output.outcomes.get("after_router"),
        Some(&NodeOutcome::Skipped)
    );
    assert_eq!(ctx.get::<u64>(&key).expect("read key"), None);
    assert!(ctx.skipped_nodes().contains("after_router"));
    assert_eq!(output.metrics.router.router_execute_total, 1);
    assert_eq!(output.metrics.router.router_failure_total, 1);
    assert_eq!(output.metrics.router.router_overlay_rollback_total, 1);
    assert_eq!(output.metrics.router.router_overlay_commit_total, 0);
}

#[test]
fn engine_router_success_commits_overlay_and_metrics() {
    let nodes = vec![
        exec_node(
            "router",
            1,
            &[],
            0,
            ExecutionNodeKind::Router {
                subgraph_id: "sg_ok".to_string(),
            },
        ),
        exec_node("after_router", 1, &["router"], 0, ExecutionNodeKind::Terminal),
    ];

    let mut ctx = RuntimeContext::default();
    let key = ContextKey {
        namespace: "router".to_string(),
        name: "ok_result".to_string(),
        version: 100,
    };
    let meta = SlotMeta {
        required: false,
        ttl_ms: None,
        sensitivity: Sensitivity::Internal,
        owner: "test".to_string(),
    };

    let output = execute_graph(
        ExecutionConfig::default(),
        nodes,
        &mut ctx,
        |spec, _, ctx| {
            if spec.dag.node_id == "router" {
                ctx.put(key.clone(), 7_u64, meta.clone())
                    .expect("write to overlay");
            }
            NodeOutcome::Success
        },
    )
    .expect("engine run should pass");

    assert_eq!(output.outcomes.get("router"), Some(&NodeOutcome::Success));
    assert_eq!(
        output.outcomes.get("after_router"),
        Some(&NodeOutcome::Success)
    );
    assert_eq!(ctx.get::<u64>(&key).expect("read key"), Some(7));
    assert_eq!(output.metrics.router.router_execute_total, 1);
    assert_eq!(output.metrics.router.router_success_total, 1);
    assert_eq!(output.metrics.router.router_overlay_commit_total, 1);
    assert_eq!(output.metrics.router.router_overlay_rollback_total, 0);
}

#[test]
fn engine_order_and_outcome_are_deterministic_across_runs() {
    let nodes = vec![
        exec_node("a", 1, &[], 1, ExecutionNodeKind::Task),
        exec_node("b", 1, &["a"], 0, ExecutionNodeKind::Terminal),
    ];

    let run_once = || {
        let mut ctx = RuntimeContext::default();
        execute_graph(
            ExecutionConfig {
                scheduler: SchedulerConfig { max_parallelism: 1 },
                ..ExecutionConfig::default()
            },
            nodes.clone(),
            &mut ctx,
            |spec, attempt, _| {
                if spec.dag.node_id == "a" && attempt == 0 {
                    NodeOutcome::Failure
                } else {
                    NodeOutcome::Success
                }
            },
        )
        .expect("engine run should pass")
    };

    let first = run_once();
    let second = run_once();

    assert_eq!(first.order, vec!["a", "a", "b"]);
    assert_eq!(first.order, second.order);
    assert_eq!(first.outcomes, second.outcomes);
    assert_eq!(first.metrics.node_retry_total, 1);
    assert_eq!(second.metrics.node_retry_total, 1);
}

#[test]
fn engine_timeout_is_enforced() {
    let nodes = vec![
        exec_node_with_policy(
            "slow",
            1,
            &[],
            RunPolicy {
                timeout_ms: 1,
                max_retries: 0,
                backoff: BackoffPolicy::None,
            },
            ExecutionNodeKind::Task,
        ),
        exec_node("after_slow", 1, &["slow"], 0, ExecutionNodeKind::Terminal),
    ];

    let mut ctx = RuntimeContext::default();
    let output = execute_graph(ExecutionConfig::default(), nodes, &mut ctx, |spec, _, _| {
        if spec.dag.node_id == "slow" {
            sleep(Duration::from_millis(5));
        }
        NodeOutcome::Success
    })
    .expect("engine run should pass");

    assert_eq!(output.outcomes.get("slow"), Some(&NodeOutcome::Timeout));
    assert_eq!(output.outcomes.get("after_slow"), Some(&NodeOutcome::Skipped));
}

#[test]
fn engine_backoff_is_applied_for_retry() {
    let nodes = vec![exec_node_with_policy(
        "retry_task",
        1,
        &[],
        RunPolicy {
            timeout_ms: 5_000,
            max_retries: 1,
            backoff: BackoffPolicy::Fixed { delay_ms: 20 },
        },
        ExecutionNodeKind::Task,
    )];

    let mut ctx = RuntimeContext::default();
    let started = Instant::now();
    let output = execute_graph(ExecutionConfig::default(), nodes, &mut ctx, |_, attempt, _| {
        if attempt == 0 {
            NodeOutcome::Failure
        } else {
            NodeOutcome::Success
        }
    })
    .expect("engine run should pass");
    let elapsed_ms = started.elapsed().as_millis();

    assert_eq!(output.order, vec!["retry_task", "retry_task"]);
    assert_eq!(output.outcomes.get("retry_task"), Some(&NodeOutcome::Success));
    assert_eq!(output.metrics.node_retry_total, 1);
    assert!(
        elapsed_ms >= 15,
        "expected elapsed >= 15ms with fixed backoff, got {elapsed_ms}ms"
    );
}

#[test]
fn engine_error_contains_execution_id() {
    let nodes = vec![ExecutionNodeSpec {
        dag: DagNodeSpec {
            node_id: "consumer".to_string(),
            priority: 1,
            consumes: vec![DagInputSpec {
                input_type: "X".to_string(),
                required: true,
                explicit_producer: None,
            }],
            produces: vec![],
            control_deps: vec![],
        },
        run_policy: RunPolicy::default(),
        kind: ExecutionNodeKind::Task,
    }];

    let mut ctx = RuntimeContext::default();
    let err = execute_graph(ExecutionConfig::default(), nodes, &mut ctx, |_, _, _| {
        NodeOutcome::Success
    })
    .expect_err("must fail in dag build phase");

    match err {
        RuntimeError::ExecutionFailed {
            execution_id,
            message,
        } => {
            assert!(execution_id.starts_with("exec-"));
            assert!(message.contains("dag build failed"));
        }
        other => panic!("expected ExecutionFailed, got {other:?}"),
    }
}
