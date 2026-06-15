use std::process::Command;
use std::fs;

fn main() {
    let src = "/root/CordisClaw/fixtures/plugins/target/debug/libgacha.so";
    let dst = "/root/CordisClaw/fixtures/artifacts/gacha.so";
    let index_path = "/root/CordisClaw/fixtures/artifacts/index.json";

    let _ = Command::new("cp").args(&[src, dst]).status();

    let hash = if let Ok(o) = Command::new("sha256sum").arg(dst).output() {
        if o.status.success() {
            String::from_utf8_lossy(&o.stdout).split_whitespace().next().unwrap_or("").to_string()
        } else { return; }
    } else { return; };

    let data = match fs::read_to_string(index_path) { Ok(d) => d, Err(_) => return };
    let mut val: serde_json::Value = match serde_json::from_str(&data) { Ok(v) => v, Err(_) => return };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs().to_string();

    // Add to topo_order
    if let Some(arr) = val["topo_order"].as_array_mut() {
        if !arr.iter().any(|v| v.as_str() == Some("gacha")) {
            arr.push(serde_json::Value::String("gacha".into()));
        }
    }

    // Find or create gacha entry
    let entries = val["entries"].as_array_mut();
    if entries.is_none() { return; }
    let entries = entries.unwrap();
    let mut found = false;
    for e in entries.iter_mut() {
        if e["plugin_path"].as_str() == Some("gacha") {
            e["sha256"] = serde_json::Value::String(hash.clone());
            e["built_at"] = serde_json::Value::String(now.clone());
            found = true;
            break;
        }
    }
    if !found {
        let new_entry = serde_json::json!({
            "plugin_path": "gacha",
            "version": "0.1.0",
            "abi_fingerprint": {
                "rustc_version": "1.85.1",
                "target_triple": "x86_64-unknown-linux-gnu",
                "crate_hash": "crate_gacha_v1",
                "api_hash": "api_v2"
            },
            "artifact_path": "gacha.so",
            "sha256": hash,
            "built_at": now,
            "parent": null,
            "required": true,
            "grants_from_parent": [],
            "docs": {
                "plugin_id": "gacha",
                "plugin_path": "gacha",
                "plugin_version": "0.1.0",
                "abi_version": 2,
                "command_name": "Gacha",
                "nodes": [
                    {
                        "id": "gacha_entry",
                        "summary": "Genshin Impact wish simulator with accurate pity and 50/50 mechanics.",
                        "input_schema": {
                            "type": "object",
                            "required": ["cmd"],
                            "properties": {
                                "cmd": { "type": "string", "description": "pull | status" },
                                "banner": { "type": "string", "description": "character(default) | weapon | standard" },
                                "count": { "type": "integer", "description": "1-100, default 1" }
                            }
                        },
                        "output_schema": {
                            "type": "object",
                            "properties": {
                                "ok": { "type": "boolean" },
                                "action": { "type": "string" },
                                "message": { "type": ["string", "null"] },
                                "results": { "type": "array", "items": { "type": "object" } },
                                "pity_5": { "type": "integer" },
                                "pity_4": { "type": "integer" },
                                "guaranteed": { "type": "boolean" }
                            }
                        },
                        "side_effects": ["accurate Genshin pity model", "soft pity starts at 73", "50/50 system"],
                        "failure_modes": ["invalid banner", "count out of range"],
                        "node_type": "router",
                        "agent_accessible": true
                    },
                    {
                        "id": "gacha_status",
                        "summary": "Check current gacha pity counters and statistics.",
                        "input_schema": {
                            "type": "object",
                            "required": ["cmd"],
                            "properties": {
                                "cmd": { "type": "string", "const": "status" }
                            }
                        },
                        "output_schema": {
                            "type": "object",
                            "properties": {
                                "ok": { "type": "boolean" },
                                "message": { "type": ["string", "null"] },
                                "pity_5": { "type": "integer" },
                                "pity_4": { "type": "integer" },
                                "guaranteed": { "type": "boolean" }
                            }
                        },
                        "side_effects": ["reads current pity state"],
                        "failure_modes": [],
                        "node_type": "router",
                        "agent_accessible": true
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
