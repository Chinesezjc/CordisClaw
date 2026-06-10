use std::process::Command;
use std::fs;

fn main() {
    let src = "/root/CordisClaw/fixtures/plugins/target/debug/libqq.so";
    let dst = "/root/CordisClaw/fixtures/artifacts/qq.so";
    let index_path = "/root/CordisClaw/fixtures/artifacts/index.json";

    // Copy the .so file
    let status = Command::new("cp")
        .args(&[src, dst])
        .status();
    match status {
        Ok(s) if s.success() => println!("cargo:warning=Copied libqq.so to artifacts/"),
        Ok(s) => println!("cargo:warning=cp exited with {}", s),
        Err(e) => println!("cargo:warning=cp failed: {}", e),
    }

    // Compute sha256 of the new .so
    let hash_output = Command::new("sha256sum")
        .arg(dst)
        .output();
    if let Ok(output) = hash_output {
        if output.status.success() {
            let hash_str = String::from_utf8_lossy(&output.stdout);
            let hash = hash_str.split_whitespace().next().unwrap_or("");
            println!("cargo:warning=New sha256: {}", hash);

            // Update index.json with the new hash and timestamp
            if let Ok(index_data) = fs::read_to_string(index_path) {
                // Replace the sha256 for qq plugin entry
                // Find the qq entry and update its sha256 and built_at
                if let Ok(mut json_val) = serde_json::from_str::<serde_json::Value>(&index_data) {
                    if let Some(entries) = json_val.get_mut("entries") {
                        if let Some(arr) = entries.as_array_mut() {
                            for entry in arr.iter_mut() {
                                if entry.get("plugin_path").and_then(|v| v.as_str()) == Some("qq") {
                                    entry["sha256"] = serde_json::Value::String(hash.to_string());
                                    entry["built_at"] = serde_json::Value::String(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs()
                                            .to_string()
                                    );
                                    // Also update generated_at
                                    json_val["generated_at"] = serde_json::Value::String(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs()
                                            .to_string()
                                    );
                                    break;
                                }
                            }
                        }
                    }
                    if let Ok(new_data) = serde_json::to_string_pretty(&json_val) {
                        if let Err(e) = fs::write(index_path, new_data) {
                            println!("cargo:warning=Failed to write index.json: {}", e);
                        } else {
                            println!("cargo:warning=Updated index.json with new hash");
                        }
                    }
                }
            }
        }
    }

    println!("cargo:rerun-if-changed=src/lib.rs");
}
