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

/// Send a message to all registered notification handlers.
pub fn send(host: &RuntimeHost, message: &str) {
    let handlers = HANDLERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    for (plugin_path, node_id) in &handlers {
        let payload = json!({
            "node_id": node_id,
            "message": message,
        });
        if let Err(e) = host.invoke(plugin_path, node_id, payload.to_string()) {
            eprintln!(
                "[notify] {plugin_path}::{node_id} failed: {e}"
            );
        }
    }
}
