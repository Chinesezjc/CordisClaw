//! Filesystem plugin — safe file I/O for the Cordis Agent.
//!
//! Nodes:
//! - `fs_read`   — read a file with line numbers
//! - `fs_write`  — write a file (whitelist: plugins/ subtree only)
//! - `fs_list`   — list directory contents
//! - `fs_search` — grep for a pattern in files
//!
//! Safety: all paths are validated by a whitelist.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint,
    PluginRequest, PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ── Whitelist ──────────────────────────────────────────────────────────

fn base_dir() -> PathBuf {
    // Use CORDIS_FIXTURES_ROOT if set, otherwise default to the process cwd.
    std::env::var("CORDIS_FIXTURES_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root/CordisClaw"))
}

/// Allowed roots for reading.
fn read_roots() -> Vec<PathBuf> {
    let base = base_dir();
    vec![
        base.clone(),
        PathBuf::from("/root/Cstar"),
        PathBuf::from("/root/PJSKBot"),
        PathBuf::from("/root/HCIProj"),
    ]
}

/// Allowed roots for writing (only plugins subtree of the fixtures root).
fn write_root() -> PathBuf {
    base_dir().join("fixtures").join("plugins")
}

fn is_allowed_read(path: &Path) -> bool {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    for root in &read_roots() {
        let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        if canonical.starts_with(&canon_root) {
            return true;
        }
    }
    // Also allow paths relative to the fixtures root.
    let base = base_dir().canonicalize().unwrap_or_else(|_| base_dir());
    canonical.starts_with(&base)
}

fn is_allowed_write(path: &Path) -> bool {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let canon_write = write_root().canonicalize().unwrap_or_else(|_| write_root());
    canonical.starts_with(&canon_write)
}

fn resolve(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(base_dir().join(p))
    }
}

// ── Request / Response ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FsRequest {
    node_id: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct FsResponse {
    ok: bool,
    node_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    lines: Option<Vec<Value>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<Value>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    matches: Option<Vec<Value>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    total_lines: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    replaced: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ── Handlers ───────────────────────────────────────────────────────────

fn handle_fs_read(req: &FsRequest) -> Result<FsResponse, String> {
    let path_str = req.path.as_deref().unwrap_or("").trim();
    if path_str.is_empty() { return Err("path is required for fs_read".into()); }
    let path = resolve(path_str)?;
    if !is_allowed_read(&path) {
        return Err("access denied: path is outside allowed read directories".into());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {path_str}: {e}"))?;
    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();
    let start = req.offset.unwrap_or(0).min(total);
    let end = req.limit.map(|n| (start + n).min(total)).unwrap_or(total);
    let lines: Vec<Value> = all_lines[start..end]
        .iter().enumerate()
        .map(|(i, l)| json!({"line": start + i + 1, "text": l}))
        .collect();
    Ok(FsResponse {
        ok: true, node_id: "fs_read".into(), path: Some(path_str.into()),
        text: None, lines: Some(lines), entries: None, matches: None,
        total_lines: Some(total), replaced: None, error: None,
    })
}

fn handle_fs_write(req: &FsRequest) -> Result<FsResponse, String> {
    let path_str = req.path.as_deref().unwrap_or("").trim();
    if path_str.is_empty() { return Err("path is required for fs_write".into()); }
    let content = req.content.as_deref().unwrap_or("").trim();
    let path = resolve(path_str)?;
    if !is_allowed_write(&path) {
        return Err("access denied: writes only allowed under plugins/".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {path_str}: {e}"))?;
    }
    std::fs::write(&path, content)
        .map_err(|e| format!("write {path_str}: {e}"))?;
    Ok(FsResponse {
        ok: true, node_id: "fs_write".into(), path: Some(path_str.into()),
        text: None, lines: None, entries: None, matches: None,
        total_lines: None, replaced: None, error: None,
    })
}

fn handle_fs_list(req: &FsRequest) -> Result<FsResponse, String> {
    let path_str = req.path.as_deref().unwrap_or(".");
    let path = resolve(path_str)?;
    if !is_allowed_read(&path) {
        return Err("access denied: path is outside allowed read directories".into());
    }
    let mut entries: Vec<Value> = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(&path)
            .map_err(|e| format!("list {path_str}: {e}"))?
        {
            let entry = entry.map_err(|e| format!("entry {path_str}: {e}"))?;
            let ft = entry.file_type().map_err(|e| format!("type {path_str}: {e}"))?;
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": if ft.is_dir() { "dir" } else { "file" },
            }));
        }
    }
    entries.sort_by(|a, b| {
        let ka = a["kind"].as_str().unwrap_or("");
        let kb = b["kind"].as_str().unwrap_or("");
        ka.cmp(kb).then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(FsResponse {
        ok: true, node_id: "fs_list".into(), path: Some(path_str.into()),
        text: None, lines: None, entries: Some(entries), matches: None,
        total_lines: None, replaced: None, error: None,
    })
}

fn handle_fs_search(req: &FsRequest) -> Result<FsResponse, String> {
    let pattern = req.pattern.as_deref().unwrap_or("").trim();
    if pattern.is_empty() { return Err("pattern is required for fs_search".into()); }
    let path_str = req.path.as_deref().unwrap_or(".");
    let path = resolve(path_str)?;
    if !is_allowed_read(&path) {
        return Err("access denied: path is outside allowed read directories".into());
    }
    let mut matches: Vec<Value> = Vec::new();
    let mut count = 0;
    let max_results = 200;
    if path.is_dir() {
        for entry in std::fs::read_dir(&path)
            .map_err(|e| format!("read_dir {path_str}: {e}"))?
            .filter_map(|e| e.ok())
        {
            if count >= max_results { break; }
            let fpath = entry.path();
            let content = match std::fs::read_to_string(&fpath) { Ok(c) => c, Err(_) => continue };
            for (lineno, line) in content.lines().enumerate() {
                if count >= max_results { break; }
                if line.contains(pattern) {
                    matches.push(json!({
                        "file": fpath.strip_prefix(&path).unwrap_or_else(|_| fpath.as_path()).to_string_lossy().into_owned(),
                        "line": lineno + 1,
                        "text": line,
                    }));
                    count += 1;
                }
            }
        }
    }
    Ok(FsResponse {
        ok: true, node_id: "fs_search".into(), path: Some(path_str.into()),
        text: None, lines: None, entries: None, matches: Some(matches),
        total_lines: None, replaced: None, error: None,
    })
}

// ── Entry point ────────────────────────────────────────────────────────

fn handle(req: &FsRequest) -> Result<FsResponse, String> {
    match req.node_id.as_str() {
        "fs_read"   => handle_fs_read(req),
        "fs_write"  => handle_fs_write(req),
        "fs_list"   => handle_fs_list(req),
        "fs_search" => handle_fs_search(req),
        other => Err(format!("unknown node_id: {other}")),
    }
}

// ── Plugin API ─────────────────────────────────────────────────────────

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "filesystem",
        "filesystem",
        "0.1.0",
        None,
        vec![
            node_doc("fs_read",  "Read a file with line numbers.",     json!({"type":"object","required":["node_id","path"],"properties":{"node_id":{"const":"fs_read"},"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}}}), json!({"type":"object","properties":{"ok":{"type":"boolean"},"lines":{"type":"array"},"total_lines":{"type":"integer"},"error":{"type":["string","null"]}}}), &["reads file from disk"], &["path outside whitelist","file not found"]),
            node_doc("fs_write", "Write a file (plugins/ subtree only).", json!({"type":"object","required":["node_id","path","content"],"properties":{"node_id":{"const":"fs_write"},"path":{"type":"string"},"content":{"type":"string"}}}), json!({"type":"object","properties":{"ok":{"type":"boolean"},"path":{"type":"string"},"error":{"type":["string","null"]}}}), &["writes file to disk","creates parent dirs"], &["path outside whitelist","permission denied"]),
            node_doc("fs_list",  "List directory contents.",           json!({"type":"object","required":["node_id"],"properties":{"node_id":{"const":"fs_list"},"path":{"type":"string"}}}), json!({"type":"object","properties":{"ok":{"type":"boolean"},"entries":{"type":"array"},"error":{"type":["string","null"]}}}), &["reads directory from disk"], &["path outside whitelist"]),
            node_doc("fs_search","Search for a pattern in files.",     json!({"type":"object","required":["node_id","pattern"],"properties":{"node_id":{"const":"fs_search"},"pattern":{"type":"string"},"path":{"type":"string"}}}), json!({"type":"object","properties":{"ok":{"type":"boolean"},"matches":{"type":"array"},"error":{"type":["string","null"]}}}), &["reads files from disk"], &["path outside whitelist"]),
        ],
        None,
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".into(),
        target_triple: "x86_64-unknown-linux-gnu".into(),
        crate_hash: "crate_filesystem_v1".into(),
        api_hash: "api_v2".into(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<FsRequest>(&req.payload)
        .map_err(|e| format!("filesystem: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&FsResponse {
            ok: false, node_id: "error".into(), path: None, text: None,
            lines: None, entries: None, matches: None,
            total_lines: None, replaced: None, error: Some(e),
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
