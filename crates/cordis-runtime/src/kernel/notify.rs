//! Generic notification bus — kernel sends messages here, plugins
//! register as handlers to deliver them via whatever channel they own
//! (QQ, Discord, Slack, etc.).  The kernel has zero knowledge of
//! delivery mechanisms.

use crate::host::RuntimeHost;
use serde_json::json;
use std::path::Path;
use std::sync::Mutex;

/// (plugin_path, node_id) pair registered to receive system notifications.
type Handler = (String, String);

static HANDLERS: Mutex<Vec<Handler>> = Mutex::new(Vec::new());

/// Load notification handler registrations from the config file.
/// Returns Vec<(plugin_path, node_id)>.
pub fn load_handlers(fixtures_root: &Path) -> Result<Vec<(String, String)>, String> {
    let path = fixtures_root.join("notify_handlers.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read: {e}"))?;
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
    let mut handlers = Vec::new();
    for entry in &entries {
        let plugin = entry
            .get("plugin_path")
            .and_then(|v| v.as_str())
            .ok_or("missing plugin_path")?;
        let node = entry
            .get("node_id")
            .and_then(|v| v.as_str())
            .ok_or("missing node_id")?;
        handlers.push((plugin.to_string(), node.to_string()));
    }
    Ok(handlers)
}

/// Register a plugin node as a notification handler.
/// P1-10: dedup so repeated calls (e.g. across reload) don't produce
/// double-deliveries. If `(plugin_path, node_id)` is already registered,
/// this is a no-op.
pub fn register(plugin_path: &str, node_id: &str) {
    let entry = (plugin_path.to_string(), node_id.to_string());
    let mut handlers = HANDLERS.lock().unwrap_or_else(|p| p.into_inner());
    if handlers.iter().any(|h| h == &entry) {
        return;
    }
    handlers.push(entry);
}

/// Remove a previously-registered handler. Used by plugin reload to drop
/// stale registrations before the new snapshot re-registers.
pub fn unregister(plugin_path: &str, node_id: &str) {
    let mut handlers = HANDLERS.lock().unwrap_or_else(|p| p.into_inner());
    handlers.retain(|(p, n)| p != plugin_path || n != node_id);
}

/// Drop every handler owned by `plugin_path` (any node). Called by reload
/// paths so a plugin that stops declaring a notify handler doesn't keep
/// receiving events from a stale registration.
pub fn unregister_plugin(plugin_path: &str) {
    let mut handlers = HANDLERS.lock().unwrap_or_else(|p| p.into_inner());
    handlers.retain(|(p, _)| p != plugin_path);
}

/// P1-10 test helper: number of registered handlers whose plugin_path
/// matches `plugin_path`. Not part of the public API — but crate-private
/// to let the notify unit tests avoid touching HANDLERS directly.
#[cfg(test)]
pub(crate) fn handler_count_for(plugin_path: &str) -> usize {
    HANDLERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|(p, _)| p == plugin_path)
        .count()
}

/// Send a message to all registered notification handlers.
pub fn send(host: &RuntimeHost, message: &str) {
    let handlers = HANDLERS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    for (plugin_path, node_id) in &handlers {
        let payload = json!({
            "node_id": node_id,
            "message": message,
        });
        if let Err(e) = host.invoke(plugin_path, node_id, payload.to_string()) {
            eprintln!("[notify] {plugin_path}::{node_id} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // P1-10 regression: register/unregister must be idempotent and
    // plugin-scoped. Tests use unique plugin paths per test to avoid
    // stepping on the process-global HANDLERS state that other tests
    // may touch.

    #[test]
    fn register_is_idempotent() {
        let p = "notify_test_p1_dedup";
        // Clean up in case a prior test left something around.
        unregister_plugin(p);
        assert_eq!(handler_count_for(p), 0);
        register(p, "n1");
        register(p, "n1");
        register(p, "n1");
        assert_eq!(handler_count_for(p), 1, "duplicate register must be no-op");
        // Different node id is a distinct entry.
        register(p, "n2");
        assert_eq!(handler_count_for(p), 2);
        unregister_plugin(p);
        assert_eq!(handler_count_for(p), 0);
    }

    #[test]
    fn unregister_specific_node_leaves_others() {
        let p = "notify_test_p1_scoped_unreg";
        unregister_plugin(p);
        register(p, "n1");
        register(p, "n2");
        assert_eq!(handler_count_for(p), 2);
        unregister(p, "n1");
        assert_eq!(handler_count_for(p), 1);
        unregister_plugin(p);
    }

    #[test]
    fn unregister_plugin_only_affects_target() {
        let p = "notify_test_p1_scoped_unreg_plugin";
        let other = "notify_test_p1_scoped_other";
        unregister_plugin(p);
        unregister_plugin(other);
        register(p, "a");
        register(p, "b");
        register(other, "a");
        assert_eq!(handler_count_for(p), 2);
        assert_eq!(handler_count_for(other), 1);
        unregister_plugin(p);
        assert_eq!(handler_count_for(p), 0);
        assert_eq!(handler_count_for(other), 1, "other plugin's handlers stay");
        unregister_plugin(other);
    }
}
