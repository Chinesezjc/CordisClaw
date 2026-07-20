//! Periodic health-check loop — every hour, directly inspects runtime
//! state and sends a report via the kernel notification bus.
//! No LLM involvement — pure code path, fast and reliable.

use crate::host::RuntimeHost;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Handle returned by `start_health_loop` — dropping it (or calling `stop`)
/// signals the loop to exit at the next tick and joins the thread.
/// P1-11: the previous `start_health_loop` had no stop signal, so the
/// thread outlived `write_shutdown_memory`/host teardown and could invoke
/// `host.status()` / `notify::send` against a torn-down runtime.
pub struct HealthLoopHandle {
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HealthLoopHandle {
    /// Signal the loop to stop and wait for the worker to exit. Called at
    /// clean shutdown so the runtime doesn't hold a live thread past
    /// teardown.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HealthLoopHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Run a health check and send the result through the notification bus.
fn run_check(host: &RuntimeHost) {
    let status = host.status();
    let zombies = host.service_registry.zombie_count();
    let healthy = zombies == 0;
    let icon = if healthy { "✅" } else { "⚠" };
    let msg = format!(
        "{icon} Self-check | {} plugins | {} nodes | {} zombies",
        status.plugin_count, status.node_count, zombies,
    );
    crate::kernel::notify::send(host, &msg);
}

pub fn start_health_loop(host: Arc<RuntimeHost>, interval_secs: u64) -> HealthLoopHandle {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();
    let thread = std::thread::spawn(move || {
        // Startup delay, interruptible.
        if !sleep_interruptible(Duration::from_secs(10), &stop_flag_clone) {
            return;
        }
        crate::kernel::notify::send(&host, "🟢 CordisClaw started");
        while !stop_flag_clone.load(Ordering::SeqCst) {
            run_check(&host);
            if !sleep_interruptible(Duration::from_secs(interval_secs), &stop_flag_clone) {
                return;
            }
        }
    });
    HealthLoopHandle {
        stop_flag,
        thread: Some(thread),
    }
}

/// Sleep for `total` in ~500 ms slices, returning `false` as soon as
/// `stop_flag` is set. Guarantees a clean shutdown latency ≤ 500 ms
/// regardless of `interval_secs`.
fn sleep_interruptible(total: Duration, stop_flag: &AtomicBool) -> bool {
    let step = Duration::from_millis(500);
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop_flag.load(Ordering::SeqCst) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(step));
    }
    !stop_flag.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1-11: `sleep_interruptible` must observe the stop flag inside a
    /// long total sleep and return early. Ensures shutdown latency is
    /// bounded by the poll interval (500ms) regardless of health-loop
    /// interval (may be an hour).
    #[test]
    fn sleep_interruptible_returns_early_on_stop_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = flag.clone();
        let handle = std::thread::spawn(move || {
            let started = Instant::now();
            // Ask for a very long sleep; the stop flag should short-
            // circuit it.
            let completed = sleep_interruptible(Duration::from_secs(60), &flag_clone);
            (completed, started.elapsed())
        });
        // Give the sleeper time to enter the loop, then flip the flag.
        std::thread::sleep(Duration::from_millis(50));
        flag.store(true, Ordering::SeqCst);
        let (completed, elapsed) = handle.join().unwrap();
        assert!(!completed, "sleep should return false when stop was set");
        assert!(
            elapsed < Duration::from_secs(2),
            "should exit within one poll window (~500ms), got {elapsed:?}"
        );
    }

    /// Sleep runs to completion when the flag is never set.
    #[test]
    fn sleep_interruptible_completes_naturally_without_stop() {
        let flag = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let completed = sleep_interruptible(Duration::from_millis(150), &flag);
        assert!(completed);
        assert!(started.elapsed() >= Duration::from_millis(140));
    }
}
