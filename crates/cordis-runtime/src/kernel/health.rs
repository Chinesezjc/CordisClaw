//! Periodic health-check loop — injects a self-check prompt into a
//! dedicated "core" agent session every hour.  The agent inspects
//! kernel status, plugin issues, and zombie services, then reports
//! findings to the configured test groups via qq_send.

use crate::host::{AgentSessionKind, RuntimeHost};
use std::sync::Arc;
use std::time::Duration;

/// Start a background thread that sends a health-check prompt to the
/// core session every `interval_secs` seconds.
pub fn start_health_loop(host: Arc<RuntimeHost>, interval_secs: u64) {
    std::thread::spawn(move || {
        // Wait for the runtime to finish booting.
        std::thread::sleep(Duration::from_secs(30));

        // Create a persistent core session.
        let core_sid = match host.agent_start(AgentSessionKind::RuntimeShell) {
            Ok(s) => s.session_id,
            Err(e) => {
                eprintln!("[health] failed to create core session: {e}");
                return;
            }
        };

        // Inject an initial system prompt that explains the role.
        let _ = host.agent_inject(
            &core_sid,
            "[system] You are the Cordis core health monitor. \
             Every hour you will receive a self-check prompt. \
             Inspect kernel status, plugin issues, and zombie services. \
             If anything is wrong, send a report to all test groups using qq_send. \
             Test group IDs are listed in qq_runtime_config.json (test_groups). \
             If everything is healthy, stay silent.",
            "Acknowledged. I will perform hourly health checks and report issues.",
        );

        loop {
            let prompt = concat!(
                "[system] Hourly self-check.\n",
                "1. Run get_kernel_status and get_kernel_issues.\n",
                "2. Run list_plugins to verify all plugins loaded.\n",
                "3. If plugin errors, zombie services, or ABI mismatches: report to test groups.\n",
                "4. If everything healthy: stay silent — do NOT send any message."
            );
            if let Err(e) = host.agent_send(&core_sid, prompt) {
                eprintln!("[health] agent_send failed: {e}");
            }
            std::thread::sleep(Duration::from_secs(interval_secs));
        }
    });
}
