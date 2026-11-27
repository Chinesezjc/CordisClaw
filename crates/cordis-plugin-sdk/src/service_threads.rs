//! Plugin-owned background threads that the runtime can **join**.
//!
//! ## Why this exists
//!
//! A plugin `handle` call may spawn threads that outlive the call: an HTTP
//! server, a message poller, a streaming response reader. The runtime opens the
//! plugin dylib, calls `handle`, and then has to decide whether it may
//! `dlclose` that dylib. If a thread is still executing dylib code when the
//! mapping goes away, the thread jumps into unmapped memory — the
//! `qq::qq_serve` SIGSEGV of 2026-11-26 (`#0 0x0 in ??`, thread entry in
//! `call_once`).
//!
//! Dropping a `JoinHandle` detaches the thread, so "the plugin spawned a
//! thread" used to be unknowable from the outside. This module turns those
//! threads into first-class, joinable objects:
//!
//! 1. the plugin spawns every long-lived thread through
//!    [`ServiceThreads::spawn`] and keeps (or drops) the returned
//!    [`ThreadTicket`];
//! 2. `export_plugin_api!` exports `_cordis_join_service_threads(timeout_ms)`
//!    and `_cordis_live_service_threads()`;
//! 3. the runtime asks how many threads are live after `handle` returns —
//!    a non-zero count keeps the dylib mapped until unload;
//! 4. at unload the runtime calls the join entry point, which sets a
//!    plugin-wide shutdown flag ([`ServiceThreads::shutdown_requested`]) and
//!    joins every registered thread. Only a `0` answer permits `dlclose`.
//!
//! Loops that sleep or poll should check [`ServiceThreads::shutdown_requested`]
//! so a join does not have to run into its timeout. A plugin that never calls
//! [`ServiceThreads::spawn`] simply reports zero live threads and its dylib can
//! be unloaded immediately.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Sleep granularity while waiting for a thread to finish.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Set by the runtime before it joins; polled by plugin loops.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Next ticket id (diagnostics only).
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Tickets dropped so far. Each thread's own ticket is dropped when the thread
/// body returns, so this counter records "thread reached its exit" — the event
/// a join must happen after.
static DROPPED_TICKETS: AtomicUsize = AtomicUsize::new(0);

/// Threads joined so far.
static JOINED_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Process-local registry of threads spawned by this plugin image.
///
/// The SDK is statically linked into each plugin dylib, so every plugin image
/// has its own registry — which is exactly the unit the runtime unloads.
fn registry() -> &'static Mutex<Vec<Arc<ThreadRecord>>> {
    static REGISTRY: OnceLock<Mutex<Vec<Arc<ThreadRecord>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn registry_lock() -> std::sync::MutexGuard<'static, Vec<Arc<ThreadRecord>>> {
    registry().lock().unwrap_or_else(PoisonError::into_inner)
}

struct ThreadRecord {
    id: u64,
    name: String,
    /// `None` once joined (or while the handle is being moved out to join).
    handle: Mutex<Option<JoinHandle<()>>>,
}

/// Handle to one registered thread.
///
/// Keep it if the plugin's own stop path wants to join the thread early (e.g.
/// an `action: "stop"` branch); drop it when the thread exits. Dropping a
/// ticket is not required for the runtime's join — the registry observes the
/// thread directly — but it records the exit for diagnostics and tests.
pub struct ThreadTicket {
    record: Arc<ThreadRecord>,
}

impl ThreadTicket {
    /// Stable id (diagnostics only).
    pub fn id(&self) -> u64 {
        self.record.id
    }

    /// Thread name passed to [`ServiceThreads::spawn`].
    pub fn name(&self) -> &str {
        &self.record.name
    }

    /// True once the thread has exited (or was already joined).
    pub fn is_finished(&self) -> bool {
        handle_finished(&self.record)
    }

    /// Join this thread and remove it from the registry. Blocks until the
    /// thread exits. Idempotent: joining twice (or after the runtime joined)
    /// reports success.
    pub fn join(&self) -> bool {
        join_record(&self.record, None)
    }
}

impl Drop for ThreadTicket {
    fn drop(&mut self) {
        DROPPED_TICKETS.fetch_add(1, Ordering::SeqCst);
    }
}

fn handle_finished(record: &ThreadRecord) -> bool {
    let handle = record.handle.lock().unwrap_or_else(PoisonError::into_inner);
    handle.as_ref().is_none_or(|handle| handle.is_finished())
}

/// Wait until `record`'s thread has exited, or until `timeout` elapses.
fn wait_finished(record: &ThreadRecord, timeout: Option<Duration>) -> bool {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    loop {
        if handle_finished(record) {
            return true;
        }
        match deadline {
            Some(deadline) if Instant::now() >= deadline => return false,
            _ => thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Join `record`'s thread if it has exited. Returns `false` when it is still
/// running after `timeout`.
fn join_record(record: &Arc<ThreadRecord>, timeout: Option<Duration>) -> bool {
    if !wait_finished(record, timeout) {
        return false;
    }
    let handle = record
        .handle
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let Some(handle) = handle else {
        return true;
    };
    // A panicking thread is already gone; its panic payload was reported by
    // the default hook, and propagating it here would poison every later
    // unload.
    let _ = handle.join();
    JOINED_THREADS.fetch_add(1, Ordering::SeqCst);
    registry_lock().retain(|live| !Arc::ptr_eq(live, record));
    true
}

/// A plugin's background-thread registry.
pub struct ServiceThreads;

impl ServiceThreads {
    /// Spawn a background thread owned by this plugin image.
    ///
    /// The thread is registered before this returns, so the runtime can never
    /// observe the dylib as "thread-free" while the thread is starting.
    pub fn spawn<F>(name: &str, body: F) -> ThreadTicket
    where
        F: FnOnce() + Send + 'static,
    {
        let record = Arc::new(ThreadRecord {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name: name.to_string(),
            handle: Mutex::new(None),
        });
        let thread_record = Arc::clone(&record);
        let handle = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                // The thread-side ticket is dropped when `body` returns, which
                // is the "thread reached its exit" event a join waits for.
                let _ticket = ThreadTicket {
                    record: thread_record,
                };
                body();
            })
            .expect("spawn plugin service thread");
        *record.handle.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        registry_lock().push(Arc::clone(&record));
        ThreadTicket { record }
    }

    /// True once the runtime asked this plugin to stop. Poll it in long-running
    /// loops so [`ServiceThreads::join_all`] does not have to wait out its
    /// timeout.
    pub fn shutdown_requested() -> bool {
        SHUTDOWN.load(Ordering::SeqCst)
    }

    /// Number of threads that have not exited yet. `0` is the only value that
    /// permits the runtime to `dlclose` this plugin image.
    pub fn live_count() -> usize {
        let mut live = registry_lock();
        live.retain(|record| !handle_finished(record));
        live.len()
    }

    /// Join every registered thread, waiting at most `timeout` in total.
    ///
    /// Sets the shutdown flag first, so cooperative loops exit without waiting
    /// for the deadline.
    pub fn join_all(timeout: Duration) -> JoinOutcome {
        SHUTDOWN.store(true, Ordering::SeqCst);
        let records: Vec<Arc<ThreadRecord>> = registry_lock().clone();
        let deadline = Instant::now() + timeout;
        let mut joined = 0usize;
        let mut unjoined = Vec::new();
        for record in records {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if join_record(&record, Some(remaining)) {
                joined += 1;
            } else {
                unjoined.push(record.name.clone());
            }
        }
        if unjoined.is_empty() {
            JoinOutcome::Joined { count: joined }
        } else {
            JoinOutcome::TimedOut { unjoined }
        }
    }

    /// Tickets dropped so far. Exposed for tests that assert the exit-before-
    /// join ordering.
    pub fn dropped_ticket_count() -> usize {
        DROPPED_TICKETS.load(Ordering::SeqCst)
    }

    /// Threads joined so far. Exposed for tests.
    pub fn joined_count() -> usize {
        JOINED_THREADS.load(Ordering::SeqCst)
    }
}

/// Result of [`ServiceThreads::join_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinOutcome {
    /// Every registered thread exited inside the timeout.
    Joined { count: usize },
    /// These threads were still running when the timeout elapsed. Their dylib
    /// must stay mapped.
    TimedOut { unjoined: Vec<String> },
}

/// C entry point exported by `export_plugin_api!`.
///
/// Returns the number of threads still running after `timeout_ms`; `0` means
/// the runtime may `dlclose` the plugin image.
pub fn join_all_timeout_ms(timeout_ms: u64) -> u64 {
    match ServiceThreads::join_all(Duration::from_millis(timeout_ms)) {
        JoinOutcome::Joined { .. } => 0,
        JoinOutcome::TimedOut { unjoined } => unjoined.len() as u64,
    }
}

/// Serializes tests that touch the process-global registry or shutdown flag.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver, Sender};

    /// Drain a receiver without blocking, after the producers are known done.
    fn drain<T>(rx: &Receiver<T>) -> Vec<T> {
        rx.try_iter().collect()
    }

    #[test]
    fn spawn_registers_a_named_live_thread() {
        let _guard = test_lock();
        let (tx, rx): (Sender<&'static str>, Receiver<&'static str>) = channel();
        let ticket = ServiceThreads::spawn("unit.live", move || {
            tx.send("ran").expect("send from thread");
        });
        assert_eq!(ticket.name(), "unit.live");
        assert!(ticket.id() > 0);
        assert!(ticket.join(), "join must wait for the thread");
        assert!(ticket.join(), "joining twice is a no-op success");
        assert_eq!(drain(&rx), vec!["ran"]);
        assert!(ticket.is_finished());
        assert_eq!(drain(&rx), Vec::<&'static str>::new());
    }

    // Two tickets, one join_all: both threads must have run to completion and
    // the registry must be empty afterwards. This is the core "stop joins every
    // registered thread" semantic.
    #[test]
    fn join_all_joins_every_registered_ticket() {
        let _guard = test_lock();
        let (tx, rx): (Sender<&'static str>, Receiver<&'static str>) = channel();
        let tx_a = tx.clone();
        let tx_b = tx;
        let a = ServiceThreads::spawn("unit.a", move || {
            tx_a.send("a").expect("send a");
        });
        let b = ServiceThreads::spawn("unit.b", move || {
            tx_b.send("b").expect("send b");
        });
        let joined_before = ServiceThreads::joined_count();
        let outcome = ServiceThreads::join_all(Duration::from_secs(5));
        assert_eq!(outcome, JoinOutcome::Joined { count: 2 });
        assert_eq!(ServiceThreads::live_count(), 0);
        assert_eq!(ServiceThreads::joined_count(), joined_before + 2);
        // Both bodies ran: the channel saw two sends, in some order.
        let mut seen = drain(&rx);
        seen.sort_unstable();
        assert_eq!(seen, vec!["a", "b"]);
        // Both tickets are finished (their `is_finished` is now stable).
        assert!(a.is_finished() && b.is_finished());
    }

    // A thread that ignores the shutdown flag must be reported, not silently
    // treated as joined: the runtime keeps its dylib mapped on `TimedOut`.
    #[test]
    fn join_all_reports_a_thread_that_ignores_shutdown() {
        let _guard = test_lock();
        let (tx, rx): (Sender<()>, Receiver<()>) = channel();
        let (started_tx, started_rx) = channel::<()>();
        let _ticket = ServiceThreads::spawn("unit.stuck", move || {
            started_tx.send(()).expect("signal start");
            // Block on a channel nobody sends to until this test's cleanup,
            // deliberately ignoring `shutdown_requested`.
            let _ = rx.recv();
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("thread must start");
        let outcome = ServiceThreads::join_all(Duration::from_millis(30));
        assert_eq!(
            outcome,
            JoinOutcome::TimedOut {
                unjoined: vec!["unit.stuck".to_string()]
            }
        );
        assert_eq!(ServiceThreads::live_count(), 1);
        // Let the thread exit so it does not outlive the test.
        tx.send(()).expect("release stuck thread");
        assert_eq!(
            ServiceThreads::join_all(Duration::from_secs(5)),
            JoinOutcome::Joined { count: 1 }
        );
        assert_eq!(ServiceThreads::live_count(), 0);
    }

    // `join_all` is what flips the shutdown flag; cooperative loops rely on it.
    #[test]
    fn join_all_requests_shutdown_for_cooperative_loops() {
        let _guard = test_lock();
        // The flag is process-global: an earlier test's `join_all` leaves it
        // set, which would make the loop exit before its first tick.
        SHUTDOWN.store(false, Ordering::SeqCst);
        let (tick_tx, tick_rx): (Sender<()>, Receiver<()>) = channel();
        let _ticket = ServiceThreads::spawn("unit.cooperative", move || {
            while !ServiceThreads::shutdown_requested() {
                // The test waits for this tick before joining, so the sleep
                // below always executes at least once (keeping the loop body
                // in the coverage report regardless of scheduling).
                let _ = tick_tx.send(());
                thread::sleep(Duration::from_millis(1));
            }
        });
        tick_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("cooperative loop must tick");
        assert_eq!(
            ServiceThreads::join_all(Duration::from_secs(5)),
            JoinOutcome::Joined { count: 1 }
        );
        assert!(ServiceThreads::shutdown_requested());
    }

    // The exit-before-join ordering: each thread drops its own ticket when its
    // body returns, and `join_all` only returns after that drop happened.
    #[test]
    fn tickets_are_dropped_before_join_all_returns() {
        let _guard = test_lock();
        let before = ServiceThreads::dropped_ticket_count();
        for _ in 0..2 {
            let _ticket = ServiceThreads::spawn("unit.order", || {
                thread::sleep(Duration::from_millis(1));
            });
        }
        assert_eq!(
            ServiceThreads::join_all(Duration::from_secs(5)),
            JoinOutcome::Joined { count: 2 }
        );
        assert!(
            ServiceThreads::dropped_ticket_count() >= before + 2,
            "each thread must drop its ticket before join_all returns"
        );
    }

    // `live_count` prunes threads that already exited but were never joined, so
    // the runtime does not keep a dylib mapped for a finished thread.
    #[test]
    fn live_count_prunes_already_finished_threads() {
        let _guard = test_lock();
        let _ticket = ServiceThreads::spawn("unit.finished", || {});
        for _ in 0..100 {
            if ServiceThreads::live_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(ServiceThreads::live_count(), 0);
    }

    // The C entry point is what the runtime calls through `dlsym`; 0 means
    // "safe to unload".
    #[test]
    fn join_all_timeout_ms_reports_unjoined_count() {
        let _guard = test_lock();
        let (tx, rx): (Sender<()>, Receiver<()>) = channel();
        let (started_tx, started_rx) = channel::<()>();
        let _ticket = ServiceThreads::spawn("unit.c-abi", move || {
            started_tx.send(()).expect("signal start");
            let _ = rx.recv();
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("thread must start");
        assert_eq!(join_all_timeout_ms(20), 1);
        tx.send(()).expect("release thread");
        assert_eq!(join_all_timeout_ms(5_000), 0);
    }

    // An unspawned registry joins to zero: a plugin that never starts a thread
    // must not keep its dylib mapped forever.
    #[test]
    fn empty_registry_joins_to_zero() {
        let _guard = test_lock();
        assert_eq!(
            ServiceThreads::join_all(Duration::from_millis(5)),
            JoinOutcome::Joined { count: 0 }
        );
        assert_eq!(join_all_timeout_ms(5), 0);
    }
}
