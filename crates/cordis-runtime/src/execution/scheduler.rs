// P2-7: the historical `run_deterministic` scheduler was superseded by
// `execution/engine.rs::execute_net`, which owns the real scheduling
// logic (including P1-3/P1-4 parallel key-sharding, P1-6 KeyedPair
// arity, P1-7 Router Timeout override, and P1-8 gate cancellation).
// `run_deterministic`, `ScheduledNode`, `ExecutionReport`, `ReadyItem`,
// and the private `cmp_ready` sibling were dead code that shipped
// alongside — deleted here. `SchedulerConfig` stays because
// `ExecutionConfig` embeds it; everything else is gone.

/// Scheduler-level tuning knobs. The actual per-batch policy lives in
/// `execution/engine.rs::execute_net`; this struct is the transport for
/// the two dimensions the caller controls.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Maximum number of transitions to batch per single-threaded iteration.
    pub max_parallelism: usize,
    /// Maximum number of correlation-key groups to execute concurrently.
    /// Transitions sharing the same key always run sequentially; transitions
    /// from different keys may run in parallel when `max_concurrency > 1`.
    /// Set to 1 (the default) for deterministic single-threaded execution.
    pub max_concurrency: usize,
}

impl SchedulerConfig {
    pub fn conservative() -> Self {
        Self {
            max_parallelism: 1,
            max_concurrency: 1,
        }
    }
}
