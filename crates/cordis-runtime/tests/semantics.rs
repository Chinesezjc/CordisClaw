use cordis_runtime::context::{
    ContextKey, ContextRead, ContextTxn, ContextWrite, RuntimeContext, Sensitivity, SlotMeta,
};
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::NodeOutcome;
use cordis_runtime::execution::engine::{
    execute_net, ExecutionConfig, ExecutionNetSpec, ExecutionTransitionKind,
    ExecutionTransitionSpec, SchedulerMode, TransitionRunResult,
};
use cordis_runtime::execution::gate::{BackoffPolicy, RunPolicy};
use cordis_runtime::execution::net::{
    build_petri_net, ArcDirection, ArcSpec, JoinPolicy, PetriNetBuildError, PetriNetSpec,
    PlaceSpec, TransitionSpec,
};
use cordis_runtime::execution::scheduler::SchedulerConfig;
use serde_json::json;
use std::thread::sleep;
use std::time::{Duration, Instant};

fn transition(
    id: &str,
    join_policy: JoinPolicy,
    priority: i32,
    kind: ExecutionTransitionKind,
) -> ExecutionTransitionSpec {
    ExecutionTransitionSpec {
        transition: TransitionSpec {
            transition_id: id.to_string(),
            priority,
            join_policy,
        },
        run_policy: RunPolicy::default(),
        kind,
        logical_group: None,
    }
}

fn transition_grouped(
    id: &str,
    join_policy: JoinPolicy,
    priority: i32,
    kind: ExecutionTransitionKind,
    group: &str,
) -> ExecutionTransitionSpec {
    let mut spec = transition(id, join_policy, priority, kind);
    spec.logical_group = Some(group.to_string());
    spec
}

fn place(id: &str) -> PlaceSpec {
    PlaceSpec {
        place_id: id.to_string(),
    }
}

fn arc_out(transition_id: &str, place_id: &str, label: Option<&str>) -> ArcSpec {
    ArcSpec {
        arc_id: format!("out::{transition_id}::{place_id}"),
        place_id: place_id.to_string(),
        transition_id: transition_id.to_string(),
        direction: ArcDirection::TransitionToPlace,
        label: label.map(|x| x.to_string()),
        required: false,
    }
}

fn arc_in(transition_id: &str, place_id: &str, label: Option<&str>) -> ArcSpec {
    ArcSpec {
        arc_id: format!("in::{transition_id}::{place_id}"),
        place_id: place_id.to_string(),
        transition_id: transition_id.to_string(),
        direction: ArcDirection::PlaceToTransition,
        label: label.map(|x| x.to_string()),
        required: true,
    }
}

#[test]
fn petri_net_build_fails_on_duplicate_transition_id() {
    let err = build_petri_net(PetriNetSpec {
        places: vec![place("p")],
        transitions: vec![
            TransitionSpec {
                transition_id: "t".to_string(),
                priority: 0,
                join_policy: JoinPolicy::AllOf,
            },
            TransitionSpec {
                transition_id: "t".to_string(),
                priority: 0,
                join_policy: JoinPolicy::AllOf,
            },
        ],
        arcs: vec![],
    })
    .expect_err("duplicate transition id should fail");

    assert!(matches!(
        err,
        PetriNetBuildError::DuplicateTransitionId { .. }
    ));
}

#[test]
fn petri_net_build_fails_on_unknown_place_arc() {
    let err = build_petri_net(PetriNetSpec {
        places: vec![place("p")],
        transitions: vec![TransitionSpec {
            transition_id: "t".to_string(),
            priority: 0,
            join_policy: JoinPolicy::AllOf,
        }],
        arcs: vec![ArcSpec {
            arc_id: "bad".to_string(),
            place_id: "missing".to_string(),
            transition_id: "t".to_string(),
            direction: ArcDirection::PlaceToTransition,
            label: None,
            required: true,
        }],
    })
    .expect_err("unknown place should fail");

    assert!(matches!(err, PetriNetBuildError::ArcPlaceNotFound { .. }));
}

#[test]
fn keyed_pair_matches_same_group_without_cross_wiring() {
    let net = ExecutionNetSpec {
        places: vec![place("pa"), place("pb"), place("pj")],
        transitions: vec![
            transition_grouped(
                "a1",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Task,
                "g1",
            ),
            transition_grouped(
                "b1",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Task,
                "g1",
            ),
            transition_grouped(
                "a2",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Task,
                "g2",
            ),
            transition_grouped(
                "b2",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Task,
                "g2",
            ),
            transition(
                "join",
                JoinPolicy::KeyedPair,
                0,
                ExecutionTransitionKind::Task,
            ),
            transition(
                "terminal",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Terminal,
            ),
        ],
        arcs: vec![
            arc_out("a1", "pa", Some("x")),
            arc_out("a2", "pa", Some("x")),
            arc_out("b1", "pb", Some("y")),
            arc_out("b2", "pb", Some("y")),
            arc_in("join", "pa", Some("x")),
            arc_in("join", "pb", Some("y")),
            arc_out("join", "pj", Some("joined")),
            arc_in("terminal", "pj", Some("joined")),
        ],
    };

    let mut ctx = RuntimeContext::default();
    let mut join_seen = 0usize;
    let output = execute_net(
        ExecutionConfig::default(),
        net,
        &mut ctx,
        |spec, _, trigger, _| {
            if spec.transition.transition_id == "join" {
                join_seen += 1;
                return TransitionRunResult {
                    outcome: NodeOutcome::Success,
                    payload: json!({ "joined": trigger.key.0 }),
                };
            }
            TransitionRunResult::from_outcome(NodeOutcome::Success)
        },
    )
    .expect("engine run should pass");

    assert_eq!(join_seen, 2, "join should fire once for each group key");
    let join_keys = output
        .keyed_outcomes
        .get("join")
        .expect("join keyed outcomes must exist");
    assert_eq!(join_keys.len(), 2);
    assert!(join_keys.keys().any(|key| key.contains("group:g1")));
    assert!(join_keys.keys().any(|key| key.contains("group:g2")));
}

#[test]
fn join_policy_first_success_marks_late_tokens_as_zombie() {
    let net = ExecutionNetSpec {
        places: vec![place("pa"), place("pb")],
        transitions: vec![
            transition_grouped(
                "a",
                JoinPolicy::AllOf,
                10,
                ExecutionTransitionKind::Task,
                "grp",
            ),
            transition_grouped(
                "b",
                JoinPolicy::AllOf,
                -1,
                ExecutionTransitionKind::Task,
                "grp",
            ),
            transition(
                "join",
                JoinPolicy::FirstSuccess,
                0,
                ExecutionTransitionKind::Task,
            ),
        ],
        arcs: vec![
            arc_out("a", "pa", Some("va")),
            arc_out("b", "pb", Some("vb")),
            arc_in("join", "pa", Some("va")),
            arc_in("join", "pb", Some("vb")),
        ],
    };

    let mut ctx = RuntimeContext::default();
    let output = execute_net(
        ExecutionConfig {
            scheduler: SchedulerConfig { max_parallelism: 1 },
            scheduler_mode: SchedulerMode::Deterministic,
        },
        net,
        &mut ctx,
        |spec, _, _, _| match spec.transition.transition_id.as_str() {
            "a" => TransitionRunResult::from_outcome(NodeOutcome::Success),
            "b" => TransitionRunResult::from_outcome(NodeOutcome::Success),
            "join" => TransitionRunResult::from_outcome(NodeOutcome::Success),
            _ => TransitionRunResult::from_outcome(NodeOutcome::Success),
        },
    )
    .expect("engine run should pass");

    assert_eq!(output.outcomes.get("join"), Some(&NodeOutcome::Success));
    assert!(output.metrics.late_token_total >= 1);
    assert!(output.metrics.zombie_token_total >= 1);
}

#[test]
fn join_policy_any_of_and_quorum_fire() {
    for policy in [JoinPolicy::AnyOf, JoinPolicy::Quorum(2)] {
        let net = ExecutionNetSpec {
            places: vec![place("p1"), place("p2")],
            transitions: vec![
                transition_grouped(
                    "u1",
                    JoinPolicy::AllOf,
                    0,
                    ExecutionTransitionKind::Task,
                    "g",
                ),
                transition_grouped(
                    "u2",
                    JoinPolicy::AllOf,
                    0,
                    ExecutionTransitionKind::Task,
                    "g",
                ),
                transition("join", policy, 0, ExecutionTransitionKind::Task),
            ],
            arcs: vec![
                arc_out("u1", "p1", Some("a")),
                arc_out("u2", "p2", Some("b")),
                arc_in("join", "p1", Some("a")),
                arc_in("join", "p2", Some("b")),
            ],
        };

        let mut ctx = RuntimeContext::default();
        let output = execute_net(
            ExecutionConfig::default(),
            net,
            &mut ctx,
            |spec, _, trigger, _| {
                if spec.transition.transition_id == "join" {
                    let has_success = trigger
                        .inputs
                        .iter()
                        .any(|input| input.token.meta.outcome == NodeOutcome::Success);
                    return TransitionRunResult::from_outcome(if has_success {
                        NodeOutcome::Success
                    } else {
                        NodeOutcome::Failure
                    });
                }
                TransitionRunResult::from_outcome(NodeOutcome::Success)
            },
        )
        .expect("engine run should pass");

        assert_eq!(output.outcomes.get("join"), Some(&NodeOutcome::Success));
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
    ctx.commit_session("s1", 0)
        .expect("first commit should pass");
    assert_eq!(ctx.session_version(), 1);

    let err = ctx.commit_session("s1", 0).expect_err("must fail by CAS");
    assert!(matches!(err, RuntimeError::CommitConflict { .. }));

    let metrics = ctx.metrics();
    assert!(metrics.context_write_total >= 1);
    assert_eq!(metrics.session_commit_conflict_total, 1);
}

#[test]
fn engine_router_failure_rolls_back_overlay() {
    let net = ExecutionNetSpec {
        places: vec![place("p_router")],
        transitions: vec![
            transition(
                "router",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Router {
                    subgraph_id: "sg1".to_string(),
                },
            ),
            transition(
                "after_router",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Terminal,
            ),
        ],
        arcs: vec![
            arc_out("router", "p_router", Some("v")),
            arc_in("after_router", "p_router", Some("v")),
        ],
    };

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

    let output = execute_net(
        ExecutionConfig::default(),
        net,
        &mut ctx,
        |spec, _, _, ctx| {
            if spec.transition.transition_id == "router" {
                ctx.put(key.clone(), 42_u64, meta.clone())
                    .expect("write to overlay");
                TransitionRunResult::from_outcome(NodeOutcome::Failure)
            } else {
                TransitionRunResult::from_outcome(NodeOutcome::Success)
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
    let net = ExecutionNetSpec {
        places: vec![place("p_router")],
        transitions: vec![
            transition(
                "router",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Router {
                    subgraph_id: "sg_ok".to_string(),
                },
            ),
            transition(
                "after_router",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Terminal,
            ),
        ],
        arcs: vec![
            arc_out("router", "p_router", Some("v")),
            arc_in("after_router", "p_router", Some("v")),
        ],
    };

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

    let output = execute_net(
        ExecutionConfig::default(),
        net,
        &mut ctx,
        |spec, _, _, ctx| {
            if spec.transition.transition_id == "router" {
                ctx.put(key.clone(), 7_u64, meta.clone())
                    .expect("write to overlay");
            }
            TransitionRunResult::from_outcome(NodeOutcome::Success)
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
fn engine_deterministic_mode_is_reproducible() {
    let make_net = || {
        let mut a = transition("a", JoinPolicy::AllOf, 1, ExecutionTransitionKind::Task);
        a.run_policy = RunPolicy {
            timeout_ms: 30_000,
            max_retries: 1,
            backoff: BackoffPolicy::None,
        };
        ExecutionNetSpec {
            places: vec![],
            transitions: vec![
                a,
                transition("b", JoinPolicy::AllOf, 1, ExecutionTransitionKind::Task),
            ],
            arcs: vec![],
        }
    };

    let run_once = || {
        let mut ctx = RuntimeContext::default();
        execute_net(
            ExecutionConfig {
                scheduler: SchedulerConfig { max_parallelism: 1 },
                scheduler_mode: SchedulerMode::Deterministic,
            },
            make_net(),
            &mut ctx,
            |spec, attempt, _, _| {
                if spec.transition.transition_id == "a" && attempt == 0 {
                    TransitionRunResult::from_outcome(NodeOutcome::Failure)
                } else {
                    TransitionRunResult::from_outcome(NodeOutcome::Success)
                }
            },
        )
        .expect("engine run should pass")
    };

    let first = run_once();
    let second = run_once();

    assert_eq!(first.order, second.order);
    assert_eq!(first.outcomes, second.outcomes);
    assert_eq!(first.metrics.node_retry_total, 1);
}

#[test]
fn engine_timeout_is_enforced() {
    let mut slow = transition("slow", JoinPolicy::AllOf, 0, ExecutionTransitionKind::Task);
    slow.run_policy = RunPolicy {
        timeout_ms: 1,
        max_retries: 0,
        backoff: BackoffPolicy::None,
    };

    let net = ExecutionNetSpec {
        places: vec![place("p")],
        transitions: vec![
            slow,
            transition(
                "after_slow",
                JoinPolicy::AllOf,
                0,
                ExecutionTransitionKind::Terminal,
            ),
        ],
        arcs: vec![
            arc_out("slow", "p", Some("v")),
            arc_in("after_slow", "p", Some("v")),
        ],
    };

    let mut ctx = RuntimeContext::default();
    let output = execute_net(
        ExecutionConfig::default(),
        net,
        &mut ctx,
        |spec, _, _, _| {
            if spec.transition.transition_id == "slow" {
                sleep(Duration::from_millis(5));
            }
            TransitionRunResult::from_outcome(NodeOutcome::Success)
        },
    )
    .expect("engine run should pass");

    assert_eq!(output.outcomes.get("slow"), Some(&NodeOutcome::Timeout));
    assert_eq!(
        output.outcomes.get("after_slow"),
        Some(&NodeOutcome::Skipped)
    );
}

#[test]
fn engine_backoff_is_applied_for_retry() {
    let mut retry_task = transition(
        "retry_task",
        JoinPolicy::AllOf,
        0,
        ExecutionTransitionKind::Task,
    );
    retry_task.run_policy = RunPolicy {
        timeout_ms: 5_000,
        max_retries: 1,
        backoff: BackoffPolicy::Fixed { delay_ms: 20 },
    };

    let net = ExecutionNetSpec {
        places: vec![],
        transitions: vec![retry_task],
        arcs: vec![],
    };

    let mut ctx = RuntimeContext::default();
    let started = Instant::now();
    let output = execute_net(
        ExecutionConfig::default(),
        net,
        &mut ctx,
        |_, attempt, _, _| {
            if attempt == 0 {
                TransitionRunResult::from_outcome(NodeOutcome::Failure)
            } else {
                TransitionRunResult::from_outcome(NodeOutcome::Success)
            }
        },
    )
    .expect("engine run should pass");
    let elapsed_ms = started.elapsed().as_millis();

    assert_eq!(output.order, vec!["retry_task", "retry_task"]);
    assert_eq!(
        output.outcomes.get("retry_task"),
        Some(&NodeOutcome::Success)
    );
    assert_eq!(output.metrics.node_retry_total, 1);
    assert!(
        elapsed_ms >= 15,
        "expected elapsed >= 15ms with fixed backoff, got {elapsed_ms}ms"
    );
}

#[test]
fn engine_error_contains_execution_id_and_net_build_message() {
    let net = ExecutionNetSpec {
        places: vec![],
        transitions: vec![transition(
            "consumer",
            JoinPolicy::AllOf,
            0,
            ExecutionTransitionKind::Task,
        )],
        arcs: vec![arc_in("consumer", "missing_place", Some("X"))],
    };

    let mut ctx = RuntimeContext::default();
    let err = execute_net(ExecutionConfig::default(), net, &mut ctx, |_, _, _, _| {
        TransitionRunResult::from_outcome(NodeOutcome::Success)
    })
    .expect_err("must fail in net build phase");

    match err {
        RuntimeError::ExecutionFailed {
            execution_id,
            message,
        } => {
            assert!(execution_id.starts_with("exec-"));
            assert!(message.contains("net build failed"));
        }
        other => panic!("expected ExecutionFailed, got {other:?}"),
    }
}
