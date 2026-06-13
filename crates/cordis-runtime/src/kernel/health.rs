//! Periodic health-check loop — every hour, directly inspects runtime
//! state and sends a report via the kernel notification bus.
//! No LLM involvement — pure code path, fast and reliable.

use crate::host::RuntimeHost;
use std::sync::Arc;
use std::time::Duration;

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

pub fn start_health_loop(host: Arc<RuntimeHost>, interval_secs: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        crate::kernel::notify::send(&host, "🟢 CordisClaw started");
        loop {
            run_check(&host);
            std::thread::sleep(Duration::from_secs(interval_secs));
        }
    });
}
