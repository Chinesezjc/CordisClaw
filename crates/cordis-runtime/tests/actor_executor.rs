use cordis_runtime::core::models::NodeOutcome;
use cordis_runtime::execution::actor::{ActorCommand, ActorExecutor};

#[test]
fn actor_executor_respects_parallel_limit_and_order() {
    let mut actor = ActorExecutor::new(2);
    actor.submit(ActorCommand::RunNode {
        node_id: "a".to_string(),
        attempt: 0,
    });
    actor.submit(ActorCommand::RunNode {
        node_id: "b".to_string(),
        attempt: 1,
    });
    actor.submit(ActorCommand::RunNode {
        node_id: "c".to_string(),
        attempt: 0,
    });

    let first = actor.dispatch_batch(|_, _| Ok(NodeOutcome::Success));
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].node_id, "a");
    assert_eq!(first[0].attempt, 0);
    assert_eq!(first[1].node_id, "b");
    assert_eq!(first[1].attempt, 1);
    assert_eq!(actor.mailbox_len(), 1);

    let second = actor.dispatch_batch(|_, _| Ok(NodeOutcome::Success));
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].node_id, "c");
    assert_eq!(actor.mailbox_len(), 0);
}
