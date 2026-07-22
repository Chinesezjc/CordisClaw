//! soul_store — SQLite-backed soul (persona) storage plugin.
//!
//! P批: overrides the kernel's file-based soul provider via the
//! capability-node convention: because this plugin declares BOTH
//! `soul_get` and `soul_set` nodes, `RuntimeHost::soul_provider()`
//! routes all soul reads/writes here instead of `data/souls/*.json`.
//! Unload/disable the plugin and the kernel falls back to files —
//! cold-start usability is never lost.
//!
//! Contract (see cordis-runtime/src/soul.rs):
//! - soul_get  {payload:{soul_key}} → {"ok":true,"soul":{...}|null}
//! - soul_set  {payload:{soul_key, soul:{persona,profile,...}}} → {"ok":true}
//!
//! DB: `$CORDIS_FIXTURES_ROOT/../data/souls.db` (business data → data/,
//! same convention as gacha).

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

// Serialize all DB access; opening per-call keeps the handle fresh
// across runtime reloads while the lock prevents writer races.
static DB_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Soul {
    #[serde(default)]
    persona: String,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    updated_at_ms: u64,
    #[serde(default)]
    updated_by: String,
}

#[derive(Debug, Deserialize)]
struct NodeRequest {
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

/// Resolve the DB location. Preference order: explicit `data_dir` from
/// the kernel's payload (authoritative — the kernel knows the workspace
/// root), then `$CORDIS_FIXTURES_ROOT/../data`, then a cwd-relative
/// fallback.
fn db_path(data_dir: Option<&str>) -> PathBuf {
    if let Some(dir) = data_dir {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("souls.db");
        }
    }
    if let Ok(root) = std::env::var("CORDIS_FIXTURES_ROOT") {
        let ws = PathBuf::from(&root)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(&root));
        return ws.join("data/souls.db");
    }
    PathBuf::from("data/souls.db")
}

fn open_db(data_dir: Option<&str>) -> Result<Connection, String> {
    let path = db_path(data_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create data dir: {e}"))?;
    }
    let conn = Connection::open(&path).map_err(|e| format!("open souls.db: {e}"))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS souls (
            soul_key TEXT PRIMARY KEY,
            persona TEXT NOT NULL DEFAULT '',
            profile TEXT,
            updated_at_ms INTEGER NOT NULL DEFAULT 0,
            updated_by TEXT NOT NULL DEFAULT ''
        )",
        [],
    )
    .map_err(|e| format!("create table: {e}"))?;
    Ok(conn)
}

fn payload_str<'a>(payload: &'a Option<Value>, key: &str) -> Option<&'a str> {
    payload.as_ref()?.get(key)?.as_str()
}

fn handle_soul_get(req: &NodeRequest) -> Result<Value, String> {
    let soul_key = payload_str(&req.payload, "soul_key").ok_or("soul_get requires payload.soul_key")?;
    let data_dir = payload_str(&req.payload, "data_dir").map(str::to_string);
    let _guard = DB_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let conn = open_db(data_dir.as_deref())?;
    let mut stmt = conn
        .prepare("SELECT persona, profile, updated_at_ms, updated_by FROM souls WHERE soul_key = ?1")
        .map_err(|e| format!("prepare: {e}"))?;
    let mut rows = stmt.query([soul_key]).map_err(|e| format!("query: {e}"))?;
    match rows.next().map_err(|e| format!("row: {e}"))? {
        Some(row) => {
            let soul = Soul {
                persona: row.get(0).map_err(|e| e.to_string())?,
                profile: row.get(1).map_err(|e| e.to_string())?,
                updated_at_ms: row.get::<_, i64>(2).map_err(|e| e.to_string())? as u64,
                updated_by: row.get(3).map_err(|e| e.to_string())?,
            };
            Ok(json!({"ok": true, "node_id": "soul_get", "soul": soul}))
        }
        None => Ok(json!({"ok": true, "node_id": "soul_get", "soul": null})),
    }
}

fn handle_soul_set(req: &NodeRequest) -> Result<Value, String> {
    let payload = req.payload.as_ref().ok_or("soul_set requires payload")?;
    let soul_key = payload
        .get("soul_key")
        .and_then(|v| v.as_str())
        .ok_or("soul_set requires payload.soul_key")?;
    if soul_key.is_empty() {
        return Err("soul_key must not be empty".to_string());
    }
    let soul: Soul = serde_json::from_value(
        payload.get("soul").cloned().ok_or("soul_set requires payload.soul")?,
    )
    .map_err(|e| format!("malformed soul: {e}"))?;
    let data_dir = payload.get("data_dir").and_then(|v| v.as_str()).map(str::to_string);
    let _guard = DB_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let conn = open_db(data_dir.as_deref())?;
    conn.execute(
        "INSERT INTO souls (soul_key, persona, profile, updated_at_ms, updated_by)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(soul_key) DO UPDATE SET
           persona = excluded.persona,
           profile = excluded.profile,
           updated_at_ms = excluded.updated_at_ms,
           updated_by = excluded.updated_by",
        rusqlite::params![
            soul_key,
            soul.persona,
            soul.profile,
            soul.updated_at_ms as i64,
            soul.updated_by,
        ],
    )
    .map_err(|e| format!("upsert: {e}"))?;
    Ok(json!({"ok": true, "node_id": "soul_set"}))
}

fn handle(req: NodeRequest) -> Result<Value, String> {
    match req.node_id.as_deref() {
        Some("soul_get") => handle_soul_get(&req),
        Some("soul_set") => handle_soul_set(&req),
        other => Err(format!("unknown soul_store node_id: {other:?}")),
    }
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "soul_store",
        "soul_store",
        "0.1.0",
        None,
        vec![
            node_doc(
                "soul_get",
                "Fetch the stored soul (persona + LLM profile reference) for a scope key. \
                 Capability node: its presence (with soul_set) overrides the kernel's \
                 file-based soul storage.",
                json!({"type":"object","required":["node_id"],"properties":{
                    "node_id":{"const":"soul_get"},
                    "payload":{"type":"object","required":["soul_key"],"properties":{
                        "soul_key":{"type":"string"}}}}}),
                json!({"type":"object","properties":{
                    "ok":{"type":"boolean"},
                    "soul":{"type":["object","null"]}}}),
                &["reads data/souls.db"],
                &["missing soul_key"],
            ),
            node_doc(
                "soul_set",
                "Upsert the soul for a scope key. Capability node paired with soul_get.",
                json!({"type":"object","required":["node_id"],"properties":{
                    "node_id":{"const":"soul_set"},
                    "payload":{"type":"object","required":["soul_key","soul"],"properties":{
                        "soul_key":{"type":"string"},
                        "soul":{"type":"object"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"}}}),
                &["writes data/souls.db"],
                &["missing soul_key", "malformed soul"],
            ),
        ],
        None,
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_soul_store_v1", "api_v2")
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<NodeRequest>(&req.payload)
        .map_err(|e| format!("soul_store: {e}"))
        .and_then(handle)
    {
        Ok(v) => json_response(&v),
        Err(e) => json_response(&json!({"ok": false, "error": e})),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(node_id: &str, payload: Value) -> NodeRequest {
        NodeRequest { node_id: Some(node_id.to_string()), payload: Some(payload) }
    }

    // CORDIS_FIXTURES_ROOT is process-global: parallel tests would race
    // on it (one test's DB visible to another). Serialize + unique dir.
    static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());
    static TEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn with_temp_db<T>(f: impl FnOnce() -> T) -> T {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let seq = TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let temp = std::env::temp_dir().join(format!(
            "soul-store-test-{}-{}",
            std::process::id(),
            seq
        ));
        let fixtures = temp.join("fixtures");
        let _ = std::fs::create_dir_all(&fixtures);
        std::env::set_var("CORDIS_FIXTURES_ROOT", &fixtures);
        let out = f();
        std::env::remove_var("CORDIS_FIXTURES_ROOT");
        let _ = std::fs::remove_dir_all(&temp);
        out
    }

    #[test]
    fn get_missing_returns_null_soul() {
        with_temp_db(|| {
            let v = handle(req("soul_get", json!({"soul_key":"nobody#private"}))).unwrap();
            assert!(v["soul"].is_null());
        });
    }

    #[test]
    fn set_then_get_roundtrip_and_upsert() {
        with_temp_db(|| {
            let soul = json!({"persona":"毒舌运维","profile":"fast","updated_at_ms":1,"updated_by":"test"});
            handle(req("soul_set", json!({"soul_key":"u#group","soul":soul}))).unwrap();
            let v = handle(req("soul_get", json!({"soul_key":"u#group"}))).unwrap();
            assert_eq!(v["soul"]["persona"], "毒舌运维");
            assert_eq!(v["soul"]["profile"], "fast");
            // upsert 覆盖
            let soul2 = json!({"persona":"温柔助手","profile":null,"updated_at_ms":2,"updated_by":"test"});
            handle(req("soul_set", json!({"soul_key":"u#group","soul":soul2}))).unwrap();
            let v = handle(req("soul_get", json!({"soul_key":"u#group"}))).unwrap();
            assert_eq!(v["soul"]["persona"], "温柔助手");
            assert!(v["soul"]["profile"].is_null());
        });
    }

    #[test]
    fn rejects_unknown_node_and_missing_key() {
        with_temp_db(|| {
            assert!(handle(req("soul_evil", json!({}))).is_err());
            assert!(handle(req("soul_set", json!({"soul_key":"","soul":{}}))).is_err());
            assert!(handle(req("soul_get", json!({}))).is_err());
        });
    }
}
