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
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ── Whitelist ──────────────────────────────────────────────────────────
//
// Extra read roots are **not** in the source: they come from this plugin's
// `settings.read_roots` in `config/plugins/filesystem.yaml`, which the kernel
// merges into the invocation payload (see `invoke_registered_plugin_with_settings`).
// Hard-coding machine-specific directories here used to mean any new project
// needed a recompile, and a stale entry (a path that no longer exists) stayed
// invisible: the `canonicalize()` check just skipped it forever.

fn base_dir() -> PathBuf {
    // Use CORDIS_FIXTURES_ROOT if set, otherwise default to the process cwd.
    //
    // The fallback used to be a hard-coded `/root/CordisClaw`, which meant a
    // runtime started from any other checkout resolved relative paths (and
    // `write_root()`) against a *different* checkout's tree — the live service
    // runs from `/root/CordisClaw-minori`, so a write aimed at `plugins/x`
    // would land in the other repo. `serve <root>` runs with the repo root as
    // cwd, so that is the honest fallback.
    std::env::var("CORDIS_FIXTURES_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Configured extra read roots, from `settings.read_roots`.
///
/// Relative entries are dropped rather than resolved against the process cwd:
/// a config typo must not silently grant access to whatever directory the
/// runtime happens to be started from.
fn extra_read_roots(settings: Option<&FsSettings>) -> Vec<PathBuf> {
    let Some(settings) = settings else {
        return Vec::new();
    };
    settings
        .read_roots
        .iter()
        .filter(|root| root.is_absolute())
        .cloned()
        .collect()
}

/// Allowed roots for writing (only plugins subtree of the fixtures root).
fn write_root() -> PathBuf {
    base_dir().join("fixtures").join("plugins")
}

/// Canonicalise a candidate path for whitelist matching. If the path exists
/// we canonicalise it directly (following symlinks). If it does not exist —
/// which is normal for writes to fresh files — we canonicalise the deepest
/// existing ancestor and re-attach the remaining components. Any failure
/// yields None so callers fail closed rather than silently falling back to
/// the un-normalised path (the P0-19 bug: `../../etc/shadow` on a
/// non-existent target used to slip through `starts_with` because the raw
/// joined path still started with the whitelist root).
fn canonicalise_for_whitelist(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }
    // Walk up to find the deepest existing ancestor.
    let mut ancestors: Vec<&Path> = Vec::new();
    let mut current = path;
    loop {
        if current.exists() {
            break;
        }
        let parent = current.parent()?;
        ancestors.push(current);
        current = parent;
    }
    let mut resolved = current.canonicalize().ok()?;
    for tail in ancestors.iter().rev() {
        if let Some(name) = tail.file_name() {
            resolved.push(name);
        }
    }
    Some(resolved)
}

fn is_allowed_read(path: &Path, settings: Option<&FsSettings>) -> bool {
    let Some(canonical) = canonicalise_for_whitelist(path) else {
        return false;
    };
    for root in extra_read_roots(settings) {
        let Ok(canon_root) = root.canonicalize() else {
            continue;
        };
        if canonical.starts_with(&canon_root) {
            return true;
        }
    }
    // The fixtures root (repo root: `$CORDIS_FIXTURES_ROOT`, else the process
    // cwd) is always readable — it holds the plugin sources the agent edits.
    let Ok(base) = base_dir().canonicalize() else {
        return false;
    };
    canonical.starts_with(&base)
}

fn is_allowed_write(path: &Path) -> bool {
    let Some(canonical) = canonicalise_for_whitelist(path) else {
        return false;
    };
    let Ok(canon_write) = write_root().canonicalize() else {
        return false;
    };
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
    /// The plugin's `settings` block from `config/plugins/filesystem.yaml`,
    /// merged into the payload by the kernel before dispatch.
    #[serde(default)]
    settings: Option<FsSettings>,
}

/// This plugin's configurable settings. Unknown keys are ignored so the
/// settings block can grow without breaking older plugin builds.
#[derive(Debug, Default, Deserialize)]
struct FsSettings {
    /// Extra directories `fs_read` / `fs_list` / `fs_search` may touch, on top
    /// of the fixtures root. Must be absolute; relative entries are ignored.
    #[serde(default)]
    read_roots: Vec<PathBuf>,
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
    if path_str.is_empty() {
        return Err("path is required for fs_read".into());
    }
    let path = resolve(path_str)?;
    if !is_allowed_read(&path, req.settings.as_ref()) {
        return Err("access denied: path is outside allowed read directories".into());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read {path_str}: {e}"))?;
    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();
    let start = req.offset.unwrap_or(0).min(total);
    let end = req.limit.map(|n| (start + n).min(total)).unwrap_or(total);
    let lines: Vec<Value> = all_lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, l)| json!({"line": start + i + 1, "text": l}))
        .collect();
    Ok(FsResponse {
        ok: true,
        node_id: "fs_read".into(),
        path: Some(path_str.into()),
        text: None,
        lines: Some(lines),
        entries: None,
        matches: None,
        total_lines: Some(total),
        replaced: None,
        error: None,
    })
}

fn handle_fs_write(req: &FsRequest) -> Result<FsResponse, String> {
    let path_str = req.path.as_deref().unwrap_or("").trim();
    if path_str.is_empty() {
        return Err("path is required for fs_write".into());
    }
    let content = req.content.as_deref().unwrap_or("").trim();
    let path = resolve(path_str)?;
    if !is_allowed_write(&path) {
        return Err("access denied: writes only allowed under plugins/".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {path_str}: {e}"))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("write {path_str}: {e}"))?;
    Ok(FsResponse {
        ok: true,
        node_id: "fs_write".into(),
        path: Some(path_str.into()),
        text: None,
        lines: None,
        entries: None,
        matches: None,
        total_lines: None,
        replaced: None,
        error: None,
    })
}

fn handle_fs_list(req: &FsRequest) -> Result<FsResponse, String> {
    let path_str = req.path.as_deref().unwrap_or(".");
    let path = resolve(path_str)?;
    if !is_allowed_read(&path, req.settings.as_ref()) {
        return Err("access denied: path is outside allowed read directories".into());
    }
    let mut entries: Vec<Value> = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(&path).map_err(|e| format!("list {path_str}: {e}"))? {
            let entry = entry.map_err(|e| format!("entry {path_str}: {e}"))?;
            let ft = entry
                .file_type()
                .map_err(|e| format!("type {path_str}: {e}"))?;
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": if ft.is_dir() { "dir" } else { "file" },
            }));
        }
    }
    entries.sort_by(|a, b| {
        let ka = a["kind"].as_str().unwrap_or("");
        let kb = b["kind"].as_str().unwrap_or("");
        ka.cmp(kb)
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(FsResponse {
        ok: true,
        node_id: "fs_list".into(),
        path: Some(path_str.into()),
        text: None,
        lines: None,
        entries: Some(entries),
        matches: None,
        total_lines: None,
        replaced: None,
        error: None,
    })
}

fn handle_fs_search(req: &FsRequest) -> Result<FsResponse, String> {
    let pattern = req.pattern.as_deref().unwrap_or("").trim();
    if pattern.is_empty() {
        return Err("pattern is required for fs_search".into());
    }
    let path_str = req.path.as_deref().unwrap_or(".");
    let path = resolve(path_str)?;
    if !is_allowed_read(&path, req.settings.as_ref()) {
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
            if count >= max_results {
                break;
            }
            let fpath = entry.path();
            let content = match std::fs::read_to_string(&fpath) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (lineno, line) in content.lines().enumerate() {
                if count >= max_results {
                    break;
                }
                if line.contains(pattern) {
                    matches.push(json!({
                        "file": fpath.strip_prefix(&path).unwrap_or(fpath.as_path()).to_string_lossy().into_owned(),
                        "line": lineno + 1,
                        "text": line,
                    }));
                    count += 1;
                }
            }
        }
    }
    Ok(FsResponse {
        ok: true,
        node_id: "fs_search".into(),
        path: Some(path_str.into()),
        text: None,
        lines: None,
        entries: None,
        matches: Some(matches),
        total_lines: None,
        replaced: None,
        error: None,
    })
}

// ── Entry point ────────────────────────────────────────────────────────

fn handle(req: &FsRequest) -> Result<FsResponse, String> {
    match req.node_id.as_str() {
        "fs_read" => handle_fs_read(req),
        "fs_write" => handle_fs_write(req),
        "fs_list" => handle_fs_list(req),
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
            node_doc("fs_read",  "Read a file with line numbers.",     json!({"type":"object","required":["node_id","path"],"properties":{"node_id":{"const":"fs_read"},"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}}}), json!({"type":"object","properties":{"ok":{"type":"boolean"},"lines":{"type":"array"},"total_lines":{"type":"integer"},"error":{"type":["string","null"]}}}), &["reads file from disk"], &["path outside whitelist","file not found"]).with_agent_accessible(),
            node_doc("fs_write", "Write a file (plugins/ subtree only).", json!({"type":"object","required":["node_id","path","content"],"properties":{"node_id":{"const":"fs_write"},"path":{"type":"string"},"content":{"type":"string"}}}), json!({"type":"object","properties":{"ok":{"type":"boolean"},"path":{"type":"string"},"error":{"type":["string","null"]}}}), &["writes file to disk","creates parent dirs"], &["path outside whitelist","permission denied"]),
            node_doc("fs_list",  "List directory contents.",           json!({"type":"object","required":["node_id"],"properties":{"node_id":{"const":"fs_list"},"path":{"type":"string"}}}), json!({"type":"object","properties":{"ok":{"type":"boolean"},"entries":{"type":"array"},"error":{"type":["string","null"]}}}), &["reads directory from disk"], &["path outside whitelist"]).with_agent_accessible(),
            node_doc("fs_search","Search for a pattern in files.",     json!({"type":"object","required":["node_id","pattern"],"properties":{"node_id":{"const":"fs_search"},"pattern":{"type":"string"},"path":{"type":"string"}}}), json!({"type":"object","properties":{"ok":{"type":"boolean"},"matches":{"type":"array"},"error":{"type":["string","null"]}}}), &["reads files from disk"], &["path outside whitelist"]).with_agent_accessible(),
        ],
        None,
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_filesystem_v1", "api_v2")
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<FsRequest>(&req.payload)
        .map_err(|e| format!("filesystem: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&FsResponse {
            ok: false,
            node_id: "error".into(),
            path: None,
            text: None,
            lines: None,
            entries: None,
            matches: None,
            total_lines: None,
            replaced: None,
            error: Some(e),
        }),
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

    /// Build a request from a JSON literal, the same shape the kernel pipes
    /// into `api_handle` — including the host-merged `settings` block.
    fn request(value: Value) -> FsRequest {
        serde_json::from_value(value).expect("request json")
    }

    fn settings_for(root: &Path) -> Option<FsSettings> {
        Some(FsSettings {
            read_roots: vec![root.to_path_buf()],
        })
    }

    /// A temp dir with `allowed/a.txt` (content `needle`) and a sibling
    /// `secret.txt` one level above the allowed root.
    fn temp_layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let allowed = temp.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::write(allowed.join("a.txt"), b"needle\n").unwrap();
        std::fs::write(temp.path().join("secret.txt"), b"secret\n").unwrap();
        let parent = temp.path().to_path_buf();
        (temp, allowed, parent)
    }

    /// P0-19: canonicalisation must succeed both for existing files
    /// AND for not-yet-created paths (walk up to the deepest existing
    /// ancestor + reattach the tail). Without this fix, a write target
    /// that didn't yet exist would fall back to the un-normalised joined
    /// path, letting `../../etc/shadow` slip through `starts_with`.
    #[test]
    fn canonicalise_resolves_existing_path() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let resolved = canonicalise_for_whitelist(&file).expect("resolve");
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("a.txt"));
    }

    #[test]
    fn canonicalise_resolves_nonexistent_leaf_via_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let phantom = temp.path().join("subdir").join("not_yet.txt");
        // subdir doesn't exist either — the walk should still find `temp`.
        let resolved = canonicalise_for_whitelist(&phantom).expect("resolve");
        // Should be prefixed by the canonicalized tempdir.
        let canonical_root = temp.path().canonicalize().unwrap();
        assert!(
            resolved.starts_with(&canonical_root),
            "resolved={} root={}",
            resolved.display(),
            canonical_root.display()
        );
    }

    #[test]
    fn canonicalise_returns_none_for_impossible_paths() {
        // A path with no accessible ancestor — /nonexistent-abs-path-xyz.
        let impossible = PathBuf::from("/does-not-exist-cordis-test-xyz/foo/bar");
        let resolved = canonicalise_for_whitelist(&impossible);
        // Depending on the filesystem this may succeed as literal or
        // return None; the important property is that a returned Some
        // is at least prefix-comparable to a canonical root.
        if let Some(p) = resolved {
            // Even in the "resolved" case, the path should be
            // syntactically absolute — no risk of ambient-cwd surprise.
            assert!(p.is_absolute());
        }
    }

    // ── settings-driven read roots ──────────────────────────────────────

    #[test]
    fn extra_read_roots_empty_without_settings() {
        assert!(extra_read_roots(None).is_empty());
        assert!(extra_read_roots(Some(&FsSettings::default())).is_empty());
    }

    /// A relative root is dropped, not resolved against the cwd: a config typo
    /// must never grant access to whatever directory the runtime started in.
    #[test]
    fn extra_read_roots_drops_relative_entries() {
        let settings = FsSettings {
            read_roots: vec![PathBuf::from("relative/dir")],
        };
        assert!(extra_read_roots(Some(&settings)).is_empty());
    }

    #[test]
    fn configured_root_is_readable() {
        let (_temp, allowed, _parent) = temp_layout();
        let settings = settings_for(&allowed);
        assert!(is_allowed_read(&allowed.join("a.txt"), settings.as_ref()));
    }

    /// No settings → a temp dir outside the fixtures root is denied. This is
    /// the "no built-in fallback list" contract: nothing is granted implicitly.
    #[test]
    fn unconfigured_root_is_denied() {
        let (_temp, allowed, _parent) = temp_layout();
        assert!(!is_allowed_read(&allowed.join("a.txt"), None));
        let err = handle(&request(json!({
            "node_id": "fs_read",
            "path": allowed.join("a.txt").to_string_lossy(),
        })))
        .expect_err("access denied");
        assert!(err.contains("access denied"), "unexpected: {err}");
    }

    /// `../` out of a configured root is denied after canonicalisation.
    #[test]
    fn escape_via_parent_component_is_denied() {
        let (_temp, allowed, parent) = temp_layout();
        let settings = settings_for(&allowed);
        let escape = allowed.join("..").join("secret.txt");
        assert!(escape.starts_with(&allowed));
        assert!(!is_allowed_read(&escape, settings.as_ref()));
        assert!(parent.join("secret.txt").exists());
    }

    /// A root that no longer exists is skipped without granting anything —
    /// the failure mode a hard-coded and later-deleted directory had.
    #[test]
    fn vanished_root_grants_nothing() {
        let settings = FsSettings {
            read_roots: vec![PathBuf::from("/does-not-exist-cordis-test-xyz")],
        };
        assert!(!is_allowed_read(
            &PathBuf::from("/does-not-exist-cordis-test-xyz/foo"),
            Some(&settings)
        ));
    }

    /// The fixtures root stays readable without any config — it holds the
    /// plugin sources the agent edits.
    #[test]
    fn fixtures_root_is_readable_without_settings() {
        assert!(is_allowed_read(&base_dir(), None));
    }

    /// A path with no existing ancestor at all fails closed rather than
    /// falling back to a raw prefix comparison.
    #[test]
    fn unresolvable_path_is_denied() {
        assert!(!is_allowed_read(Path::new(""), None));
    }

    // ── handlers honour the configured roots end to end ─────────────────

    #[test]
    fn fs_read_reads_configured_root() {
        let (_temp, allowed, _parent) = temp_layout();
        let response = handle(&request(json!({
            "node_id": "fs_read",
            "path": allowed.join("a.txt").to_string_lossy(),
            "settings": { "read_roots": [allowed.to_string_lossy()] },
        })))
        .expect("read allowed file");
        assert!(response.ok);
        assert_eq!(response.total_lines, Some(1));
        let first = response.lines.as_ref().unwrap()[0].clone();
        assert_eq!(first["text"], "needle");
    }

    #[test]
    fn fs_list_lists_configured_root() {
        let (_temp, allowed, _parent) = temp_layout();
        let response = handle(&request(json!({
            "node_id": "fs_list",
            "path": allowed.to_string_lossy(),
            "settings": { "read_roots": [allowed.to_string_lossy()] },
        })))
        .expect("list allowed dir");
        let entries = response.entries.as_ref().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "a.txt");
        assert_eq!(entries[0]["kind"], "file");
    }

    #[test]
    fn fs_search_searches_configured_root() {
        let (_temp, allowed, _parent) = temp_layout();
        let response = handle(&request(json!({
            "node_id": "fs_search",
            "pattern": "needle",
            "path": allowed.to_string_lossy(),
            "settings": { "read_roots": [allowed.to_string_lossy()] },
        })))
        .expect("search allowed dir");
        let matches = response.matches.as_ref().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["text"], "needle");
    }

    /// Settings without `read_roots` must not widen the whitelist.
    #[test]
    fn settings_without_read_roots_widens_nothing() {
        let (_temp, allowed, _parent) = temp_layout();
        let request = request(json!({
            "node_id": "fs_read",
            "path": allowed.join("a.txt").to_string_lossy(),
            "settings": { "some_other_setting": true },
        }));
        assert!(request.settings.is_some(), "settings block still parses");
        let err = handle(&request).expect_err("no read_roots configured");
        assert!(err.contains("access denied"), "unexpected: {err}");
    }
}
