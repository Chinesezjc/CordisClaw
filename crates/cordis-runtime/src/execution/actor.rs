//! Actor-style execution primitives.
//! This keeps a mailbox-based run model for node execution, while preserving
//! deterministic ordering decisions from the scheduler.

use crate::core::error::RuntimeError;
use crate::core::models::NodeOutcome;
use std::collections::VecDeque;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorCommand {
    RunNode { node_id: String, attempt: u32 },
}

#[derive(Debug)]
pub struct ActorEvent {
    pub node_id: String,
    pub attempt: u32,
    pub elapsed_ms: u128,
    pub result: Result<NodeOutcome, RuntimeError>,
}

#[derive(Debug, Clone)]
pub struct ActorExecutor {
    max_parallelism: usize,
    mailbox: VecDeque<ActorCommand>,
}

impl ActorExecutor {
    pub fn new(max_parallelism: usize) -> Self {
        Self {
            max_parallelism: max_parallelism.max(1),
            mailbox: VecDeque::new(),
        }
    }

    pub fn submit(&mut self, command: ActorCommand) {
        self.mailbox.push_back(command);
    }

    pub fn mailbox_len(&self) -> usize {
        self.mailbox.len()
    }

    pub fn dispatch_batch<F>(&mut self, mut run: F) -> Vec<ActorEvent>
    where
        F: FnMut(&str, u32) -> Result<NodeOutcome, RuntimeError>,
    {
        let mut events = Vec::new();
        for _ in 0..self.max_parallelism {
            let Some(command) = self.mailbox.pop_front() else {
                break;
            };
            match command {
                ActorCommand::RunNode { node_id, attempt } => {
                    let started_at = Instant::now();
                    let result = run(&node_id, attempt);
                    events.push(ActorEvent {
                        node_id,
                        attempt,
                        elapsed_ms: started_at.elapsed().as_millis(),
                        result,
                    });
                }
            }
        }
        events
    }
}
