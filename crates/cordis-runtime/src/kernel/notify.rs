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

    #[test]
    fn load_handlers_parses_valid_config() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("notify_handlers.json"),
            r#"[{"plugin_path":"qq","node_id":"send"},{"plugin_path":"slack","node_id":"post"}]"#,
        )
        .expect("write config");
        let handlers = load_handlers(dir.path()).expect("valid config parses");
        assert_eq!(
            handlers,
            vec![
                ("qq".to_string(), "send".to_string()),
                ("slack".to_string(), "post".to_string()),
            ]
        );
    }

    #[test]
    fn load_handlers_missing_file_is_read_error() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let err = load_handlers(dir.path()).expect_err("missing file must error");
        assert!(err.starts_with("read:"), "err: {err}");
    }

    #[test]
    fn load_handlers_invalid_json_is_parse_error() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("notify_handlers.json"), "{not an array")
            .expect("write config");
        let err = load_handlers(dir.path()).expect_err("bad json must error");
        assert!(err.starts_with("parse:"), "err: {err}");
    }

    #[test]
    fn load_handlers_missing_plugin_path_field_errors() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("notify_handlers.json"),
            r#"[{"node_id":"send"}]"#,
        )
        .expect("write config");
        let err = load_handlers(dir.path()).expect_err("missing plugin_path must error");
        assert_eq!(err, "missing plugin_path");
    }

    #[test]
    fn load_handlers_missing_node_id_field_errors() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("notify_handlers.json"),
            r#"[{"plugin_path":"qq"}]"#,
        )
        .expect("write config");
        let err = load_handlers(dir.path()).expect_err("missing node_id must error");
        assert_eq!(err, "missing node_id");
    }

    /// `send` fans out to every registered handler and swallows per-handler
    /// invoke errors (they only log). With a handler pointing at a plugin the
    /// host does not know about, `host.invoke` errors and `send` must still
    /// return without panicking.
    #[test]
    fn send_delivers_to_registered_handlers_and_tolerates_invoke_errors() {
        use crate::host::RuntimeHost;
        let dir = tempfile::TempDir::new().expect("tempdir");
        let artifacts = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifacts).expect("artifacts dir");
        std::fs::write(
            artifacts.join("index.json"),
            r#"{"generated_at":"1970-01-01T00:00:00Z","topo_order":[],"entries":[]}"#,
        )
        .expect("write empty index");
        let host = RuntimeHost::boot(dir.path()).expect("empty host should boot");

        let p = "notify_test_send_unknown_plugin";
        unregister_plugin(p);
        // No such plugin is loaded, so invoke returns Err → the eprintln
        // branch runs. send must not propagate or panic.
        register(p, "handler_node");
        send(&host, "hello world");
        // A send with zero matching handlers is also a no-op.
        unregister_plugin(p);
        send(&host, "no handlers now");
        assert_eq!(handler_count_for(p), 0);
    }
}
