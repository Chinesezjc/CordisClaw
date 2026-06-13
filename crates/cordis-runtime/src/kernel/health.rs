//! Periodic health-check loop — every hour, directly inspects runtime
//! state and sends a report to configured test groups via qq_send.
//! No LLM involvement — pure code path, fast and reliable.

use crate::host::RuntimeHost;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Run a health check: read snapshot info + zombie count, then send a
/// report to all configured test groups.
fn run_check(host: &RuntimeHost) {
    let status = host.status();
    let zombies = host.service_registry.zombie_count();
    let healthy = zombies == 0;
    let icon = if healthy { "✅" } else { "⚠" };
    let msg = format!(
        "{icon} Self-check | {} plugins | {} nodes | {} zombies",
        status.plugin_count, status.node_count, zombies,
    );

    let test_groups = read_test_groups();
    for gid in &test_groups {
        let payload = json!({
            "node_id": "qq_send",
            "target": format!("group:{gid}"),
            "message": &msg,
        });
        if let Err(e) = host.invoke("qq", "qq_send", payload.to_string()) {
            eprintln!("[health] qq_send to {gid} failed: {e}");
        }
    }
}

fn read_test_groups() -> Vec<String> {
    let path = "/root/CordisClaw/fixtures/.cordis-drafts/qq_runtime_config.json";
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec![];
    };
    config
        .get("test_groups")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn start_health_loop(host: Arc<RuntimeHost>, interval_secs: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        loop {
            run_check(&host);
            std::thread::sleep(Duration::from_secs(interval_secs));
        }
    });
}
