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

    /// Boot a minimal host from a fixtures dir containing only an empty
    /// artifact index (zero plugins). No dylib load → boots on any host OS
    /// (no x86_64-linux dependency).
    fn boot_empty_host() -> Arc<RuntimeHost> {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let artifacts = temp.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).expect("artifacts dir");
        std::fs::write(
            artifacts.join("index.json"),
            r#"{"generated_at":"1970-01-01T00:00:00Z","topo_order":[],"entries":[]}"#,
        )
        .expect("write empty index");
        // Leak the TempDir so the snapshot root survives for the host's
        // lifetime; the test process exit reclaims it.
        let path = temp.keep();
        Arc::new(RuntimeHost::boot(&path).expect("empty host should boot"))
    }

    /// `run_check` inspects live status and formats a report. With zero
    /// zombies it must pick the healthy icon; the notify send is a no-op
    /// because no handler is registered for this host.
    #[test]
    fn run_check_inspects_status_without_panicking() {
        let host = boot_empty_host();
        // Sanity: an empty host reports zero zombies → healthy branch.
        assert_eq!(host.service_registry.zombie_count(), 0);
        // Exercise the full code path (status + format + notify::send).
        run_check(&host);
    }

    /// `start_health_loop` must spawn a worker and, when stopped before the
    /// 10s startup delay elapses, exit cleanly via the interruptible sleep
    /// without ever hitting the periodic check.
    #[test]
    fn start_health_loop_stops_promptly_during_startup_delay() {
        let host = boot_empty_host();
        let started = Instant::now();
        let handle = start_health_loop(host, 3600);
        // Give the worker a moment to enter the startup sleep, then stop.
        std::thread::sleep(Duration::from_millis(30));
        handle.stop();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stop() should join within one poll window, got {:?}",
            started.elapsed()
        );
    }

    /// Dropping the handle (instead of calling `stop`) also signals the loop
    /// and joins the worker — the P1-11 teardown guarantee.
    #[test]
    fn dropping_handle_signals_and_joins() {
        let host = boot_empty_host();
        let handle = start_health_loop(host, 3600);
        std::thread::sleep(Duration::from_millis(30));
        drop(handle); // Drop impl sets the flag and joins.
    }
}
