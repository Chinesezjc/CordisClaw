//! Git plugin — version control operations for the agent.
//!
//! Nodes:
//! - `git_diff`   — show working tree diff (unified format)
//! - `git_log`    — show recent commit history
//! - `git_status` — show working tree status (short format)
//! - `git_commit` — stage and commit changes
//!
//! Safety: all operations are scoped to the fixtures root. Dangerous operations
//! (push, force-push, amend, rebase) are explicitly blocked.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_OUTPUT_CHARS: usize = 10000;

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitRequest {
    /// "git_diff" | "git_log" | "git_status" | "git_commit"
    node_id: String,

    #[serde(default)]
    fixtures_root: Option<String>,

    #[serde(default)]
    path: Option<String>,

    #[serde(default)]
    staged: Option<bool>,

    #[serde(default)]
    max_count: Option<usize>,

    #[serde(default)]
    message: Option<String>,

    #[serde(default)]
    paths: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct GitResponse {
    ok: bool,
    node_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    log: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    committed: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

fn run_git(repo_root: &Path, args: &[&str]) -> Result<(String, String), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|e| format!("execute git: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Err(format!("git {}: {stderr}", args.join(" ")));
    }

    Ok((stdout, stderr))
}

fn truncate(s: &str) -> String {
    s.chars().take(MAX_OUTPUT_CHARS).collect()
}

fn validate_commit_message(msg: &str) -> Result<(), String> {
    if msg.trim().is_empty() {
        return Err("commit message must not be empty".to_string());
    }
    let lower = msg.to_lowercase();
    for forbidden in &["--force", "--no-verify", "--allow-empty"] {
        if lower.contains(forbidden) {
            return Err(format!("forbidden flag in message: {forbidden}"));
        }
    }
    Ok(())
}

fn resolve_root(req_root: Option<&str>) -> Result<PathBuf, String> {
    match req_root {
        Some(r) => {
            let p = Path::new(r);
            if !p.is_dir() {
                return Err(format!("fixtures_root is not a directory: {r}"));
            }
            Ok(p.to_path_buf())
        }
        None => Err("fixtures_root is required".to_string()),
    }
}

fn validate_path_in_root(root: &Path, rel: &str) -> Result<(), String> {
    let resolved = root.join(rel);
    let canonical = resolved.canonicalize().unwrap_or(resolved.clone());
    let canonical_root = root.canonicalize().unwrap_or(root.to_path_buf());
    if !canonical.starts_with(&canonical_root) {
        return Err(format!("path escapes fixtures_root: {rel}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Node handlers
// ---------------------------------------------------------------------------

fn handle_diff(req: &GitRequest) -> Result<GitResponse, String> {
    let root = resolve_root(req.fixtures_root.as_deref())?;
    let mut args = vec!["diff"];
    if req.staged.unwrap_or(false) {
        args.push("--cached");
    }
    args.push("--");
    if let Some(p) = req.path.as_deref() {
        validate_path_in_root(&root, p)?;
        args.push(p);
    }

    let (stdout, _) = run_git(&root, &args)?;
    Ok(GitResponse {
        ok: true,
        node_id: "git_diff".to_string(),
        diff: Some(truncate(&stdout)),
        log: None,
        status: None,
        committed: None,
        head: None,
        error: None,
    })
}

fn handle_log(req: &GitRequest) -> Result<GitResponse, String> {
    let root = resolve_root(req.fixtures_root.as_deref())?;
    let n = req.max_count.unwrap_or(10).to_string();
    let mut args = vec!["log", "--oneline", "-n", &n];
    if let Some(p) = req.path.as_deref() {
        validate_path_in_root(&root, p)?;
        args.push("--");
        args.push(p);
    }

    let (stdout, _) = run_git(&root, &args)?;
    Ok(GitResponse {
        ok: true,
        node_id: "git_log".to_string(),
        diff: None,
        log: Some(truncate(&stdout)),
        status: None,
        committed: None,
        head: None,
        error: None,
    })
}

fn handle_status(req: &GitRequest) -> Result<GitResponse, String> {
    let root = resolve_root(req.fixtures_root.as_deref())?;
    let (stdout, _) = run_git(&root, &["status", "--short"])?;
    Ok(GitResponse {
        ok: true,
        node_id: "git_status".to_string(),
        diff: None,
        log: None,
        status: Some(truncate(&stdout)),
        committed: None,
        head: None,
        error: None,
    })
}

fn handle_commit(req: &GitRequest) -> Result<GitResponse, String> {
    let message = req.message.as_deref().unwrap_or("").trim();
    validate_commit_message(message)?;

    let lower = message.to_lowercase();
    for forbidden in &["push", "amend", "force", "rebase"] {
        if lower.contains(forbidden) {
            return Err(format!("forbidden operation: {forbidden}"));
        }
    }

    let root = resolve_root(req.fixtures_root.as_deref())?;

    // Stage.
    if let Some(file_paths) = req.paths.as_deref() {
        if file_paths.is_empty() {
            return Err("paths must not be empty".to_string());
        }
        for p in file_paths {
            validate_path_in_root(&root, p)?;
        }
        let mut args = vec!["add"];
        args.extend(file_paths.iter().map(|s| s.as_str()));
        run_git(&root, &args)?;
    } else {
        run_git(&root, &["add", "-A"])?;
    }

    // Commit.
    run_git(&root, &["commit", "-m", message])?;
    let (head, _) = run_git(&root, &["rev-parse", "HEAD"])?;

    Ok(GitResponse {
        ok: true,
        node_id: "git_commit".to_string(),
        diff: None,
        log: None,
        status: None,
        committed: Some(true),
        head: Some(head.trim().to_string()),
        error: None,
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn handle(req: &GitRequest) -> Result<GitResponse, String> {
    match req.node_id.as_str() {
        "git_diff" => handle_diff(req),
        "git_log" => handle_log(req),
        "git_status" => handle_status(req),
        "git_commit" => handle_commit(req),
        other => Err(format!("unknown node_id: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Plugin API exports
// ---------------------------------------------------------------------------

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "git",
        "git",
        "0.1.0",
        None,
        vec![
            node_doc(
                "git_diff",
                "Show git working tree diff (unified format). Use to review changes before committing.",
                json!({
                    "type": "object",
                    "required": ["node_id", "fixtures_root"],
                    "properties": {
                        "node_id": { "type": "string", "const": "git_diff" },
                        "fixtures_root": { "type": "string", "description": "Path to the git repository root" },
                        "path": { "type": "string", "description": "Optional: limit diff to a specific file or directory" },
                        "staged": { "type": "boolean", "description": "Show staged changes only (default false)" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "diff": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["reads git working tree"],
                &["not a git repository", "path escapes root"],
            ),
            node_doc(
                "git_log",
                "Show recent git commit history (oneline format).",
                json!({
                    "type": "object",
                    "required": ["node_id", "fixtures_root"],
                    "properties": {
                        "node_id": { "type": "string", "const": "git_log" },
                        "fixtures_root": { "type": "string", "description": "Path to the git repository root" },
                        "max_count": { "type": "integer", "description": "Max commits to show (default 10)" },
                        "path": { "type": "string", "description": "Optional: limit log to a specific file" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "log": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["reads git history"],
                &["not a git repository"],
            ),
            node_doc(
                "git_status",
                "Show git working tree status (short format).",
                json!({
                    "type": "object",
                    "required": ["node_id", "fixtures_root"],
                    "properties": {
                        "node_id": { "type": "string", "const": "git_status" },
                        "fixtures_root": { "type": "string", "description": "Path to the git repository root" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "status": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["reads git working tree"],
                &["not a git repository"],
            ),
            node_doc(
                "git_commit",
                "Stage and commit changes. Pass specific file paths or omit to commit all changes. Dangerous operations (push, force, amend) are blocked.",
                json!({
                    "type": "object",
                    "required": ["node_id", "fixtures_root", "message"],
                    "properties": {
                        "node_id": { "type": "string", "const": "git_commit" },
                        "fixtures_root": { "type": "string", "description": "Path to the git repository root" },
                        "message": { "type": "string", "description": "Commit message" },
                        "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional: specific files to add and commit" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "committed": { "type": "boolean" },
                        "head": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["modifies git history (local only)"],
                &["commit message empty", "forbidden operation", "path escapes root", "not a git repository"],
            ),
        ],
    None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_git_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<GitRequest>(&req.payload)
        .map_err(|e| format!("git plugin: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&GitResponse {
            ok: false,
            node_id: "error".to_string(),
            diff: None,
            log: None,
            status: None,
            committed: None,
            head: None,
            error: Some(e),
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
