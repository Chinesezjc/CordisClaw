use std::fs;
use std::process::Command;

// Mirrors gacha/build.rs: copy the built dylib into fixtures/artifacts
// and self-register in index.json so the loader discovers the plugin.
fn main() {
    let src = "/root/CordisClaw/fixtures/plugins/target/debug/libsoul_store.so";
    let dst = "/root/CordisClaw/fixtures/artifacts/soul_store.so";
    let index_path = "/root/CordisClaw/fixtures/artifacts/index.json";

    let _ = Command::new("cp").args([src, dst]).status();

    let hash = if let Ok(o) = Command::new("sha256sum").arg(dst).output() {
        if o.status.success() {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            return;
        }
    } else {
        return;
    };

    let data = match fs::read_to_string(index_path) {
        Ok(d) => d,
        Err(_) => return,
    };
    let mut val: serde_json::Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();

    if let Some(arr) = val["topo_order"].as_array_mut() {
        if !arr.iter().any(|v| v.as_str() == Some("soul_store")) {
            arr.push(serde_json::Value::String("soul_store".into()));
        }
    }

    let entries = match val["entries"].as_array_mut() {
        Some(e) => e,
        None => return,
    };
    let mut found = false;
    for e in entries.iter_mut() {
        if e["plugin_path"].as_str() == Some("soul_store") {
            e["sha256"] = serde_json::Value::String(hash.clone());
            e["built_at"] = serde_json::Value::String(now.clone());
            found = true;
            break;
        }
    }
    if !found {
        let new_entry = serde_json::json!({
            "plugin_path": "soul_store",
            "version": "0.1.0",
            "abi_fingerprint": {
                "rustc_version": "1.85.1",
                "target_triple": "x86_64-unknown-linux-gnu",
                "crate_hash": "crate_soul_store_v1",
                "api_hash": "api_v2"
            },
            "artifact_path": "soul_store.so",
            "sha256": hash,
            "built_at": now,
            "parent": null,
            "required": false,
            "grants_from_parent": [],
            "docs": {
                "plugin_id": "soul_store",
                "plugin_path": "soul_store",
                "plugin_version": "0.1.0",
                "abi_version": 2,
                "command_name": null,
                "nodes": [
                    {
                        "id": "soul_get",
                        "summary": "Fetch stored soul (persona + LLM profile) for a scope key. Capability node overriding the kernel file soul store.",
                        "input_schema": {
                            "type": "object",
                            "required": ["node_id"],
                            "properties": {
                                "node_id": { "const": "soul_get" },
                                "payload": {
                                    "type": "object",
                                    "required": ["soul_key"],
                                    "properties": { "soul_key": { "type": "string" } }
                                }
                            }
                        },
                        "output_schema": {
                            "type": "object",
                            "properties": {
                                "ok": { "type": "boolean" },
                                "soul": { "type": ["object", "null"] }
                            }
                        },
                        "side_effects": ["reads data/souls.db"],
                        "failure_modes": ["missing soul_key"],
                        "node_type": "router",
                        "agent_accessible": false
                    },
                    {
                        "id": "soul_set",
                        "summary": "Upsert the soul for a scope key. Capability node paired with soul_get.",
                        "input_schema": {
                            "type": "object",
                            "required": ["node_id"],
                            "properties": {
                                "node_id": { "const": "soul_set" },
                                "payload": {
                                    "type": "object",
                                    "required": ["soul_key", "soul"],
                                    "properties": {
                                        "soul_key": { "type": "string" },
                                        "soul": { "type": "object" }
                                    }
                                }
                            }
                        },
                        "output_schema": {
                            "type": "object",
                            "properties": { "ok": { "type": "boolean" } }
                        },
                        "side_effects": ["writes data/souls.db"],
                        "failure_modes": ["missing soul_key", "malformed soul"],
                        "node_type": "router",
                        "agent_accessible": false
                    }
                ],
                "system_hint": null
            },
            "exports": [],
            "execution": null,
            "artifact_kind": "dylib",
            "build_fingerprint": "0000000000000000000000000000000000000000000000000000000000000000",
            "input_probe": { "files": [] },
            "local_path_deps": ["crates/cordis-plugin-sdk"]
        });
        entries.push(new_entry);
    }
    val["generated_at"] = serde_json::Value::String(now);
    if let Ok(new_data) = serde_json::to_string_pretty(&val) {
        let _ = fs::write(index_path, new_data);
    }
    println!("cargo:rerun-if-changed=src/lib.rs");
}
