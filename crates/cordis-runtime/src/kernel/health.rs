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
             On every check, inspect kernel status, plugin issues, and zombie services. \
             ALWAYS send a report to all test groups (qq_runtime_config.json: test_groups). \
             Healthy: send a brief all-clear. Unhealthy: send details of what's wrong.",
            "Acknowledged. I will always report results, healthy or not.",
        );

        // Run the first check immediately, then loop.
        loop {
            let prompt = concat!(
                "[system] Self-check.\n",
                "1. Run get_kernel_status and get_kernel_issues.\n",
                "2. Run list_plugins to verify all plugins loaded.\n",
                "3. Send a brief report to ALL test groups via qq_send.\n",
                "   Healthy example: '✅ Self-check OK | 18 plugins | 0 issues | 0 zombies'\n",
                "   Unhealthy example: '⚠ Self-check: 2 plugin errors, 1 zombie service'"
            );
            match host.agent_send(&core_sid, prompt) {
                Ok(reply) => eprintln!(
                    "[health] check complete: {} chars, {} tool calls",
                    reply.content.len(),
                    reply.tool_events.len(),
                ),
                Err(e) => eprintln!("[health] agent_send failed: {e}"),
            }
            std::thread::sleep(Duration::from_secs(interval_secs));
        }
    });
}
