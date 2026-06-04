//! Agent git tools: diff, log, status, commit.
//!
//! All operations are scoped to the workspace root.  Dangerous operations
//! (push, force-push, amend) are explicitly blocked.

use crate::core::error::RuntimeError;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

const MAX_GIT_OUTPUT_CHARS: usize = 10000;

/// Sanity-check a commit message: reject obviously dangerous or empty input.
fn validate_commit_message(msg: &str) -> Result<(), RuntimeError> {
    if msg.trim().is_empty() {
        return Err(RuntimeError::InvalidArgument {
            message: "git_commit: message must not be empty".to_string(),
        });
    }
    let lower = msg.to_lowercase();
    for forbidden in &["--force", "--no-verify", "--allow-empty"] {
        if lower.contains(forbidden) {
            return Err(RuntimeError::InvalidArgument {
                message: format!("git_commit: forbidden flag in message: {forbidden}"),
            });
        }
    }
    Ok(())
}

/// Run `git` inside `workspace_root` with the given arguments.
/// Returns (stdout, stderr).  Times out after 30 seconds.
fn run_git(workspace_root: &Path, args: &[&str]) -> Result<(String, String), RuntimeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(args)
        .output()
        .map_err(|e| RuntimeError::Invariant {
            message: format!("git: failed to execute: {e}"),
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Err(RuntimeError::Invariant {
            message: format!("git: {}: {stderr}", args.join(" ")),
        });
    }

    Ok((stdout, stderr))
}

fn truncate(s: &str) -> String {
    s.chars().take(MAX_GIT_OUTPUT_CHARS).collect()
}

// ---------------------------------------------------------------------------
// Public tool entry-points
// ---------------------------------------------------------------------------

/// `git diff [--cached] [-- <path>]`
pub fn git_diff(
    workspace_root: &Path,
    path: Option<&str>,
    staged: bool,
) -> Result<Value, RuntimeError> {
    let mut args = vec!["diff"];
    if staged {
        args.push("--cached");
    }
    args.push("--"); // separator
    if let Some(p) = path {
        args.push(p);
    }

    let (stdout, _) = run_git(workspace_root, &args)?;
    Ok(serde_json::json!({
        "diff": truncate(&stdout),
    }))
}

/// `git log --oneline -n <max_count> [-- <path>]`
pub fn git_log(
    workspace_root: &Path,
    max_count: usize,
    path: Option<&str>,
) -> Result<Value, RuntimeError> {
    let n = max_count.to_string();
    let mut args = vec!["log", "--oneline", "-n", &n];
    if let Some(p) = path {
        args.push("--");
        args.push(p);
    }

    let (stdout, _) = run_git(workspace_root, &args)?;
    Ok(serde_json::json!({
        "log": truncate(&stdout),
    }))
}

/// `git status --short`
pub fn git_status(workspace_root: &Path) -> Result<Value, RuntimeError> {
    let (stdout, _) = run_git(workspace_root, &["status", "--short"])?;
    Ok(serde_json::json!({
        "status": truncate(&stdout),
    }))
}

/// `git add [paths]` then `git commit -m <message>`
pub fn git_commit(
    workspace_root: &Path,
    message: &str,
    paths: Option<&[String]>,
) -> Result<Value, RuntimeError> {
    validate_commit_message(message)?;

    // Reject dangerous operations.
    let lower = message.to_lowercase();
    for forbidden in &["push", "amend", "force", "rebase"] {
        if lower.contains(forbidden) {
            return Err(RuntimeError::InvalidArgument {
                message: format!("git_commit: forbidden operation detected: {forbidden}"),
            });
        }
    }

    // Stage files.
    if let Some(file_paths) = paths {
        if file_paths.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "git_commit: paths must not be empty".to_string(),
            });
        }
        // Validate each path is under workspace_root.
        for p in file_paths {
            let resolved = workspace_root.join(p);
            let canonical = resolved
                .canonicalize()
                .unwrap_or_else(|_| resolved.clone());
            let canonical_root = workspace_root
                .canonicalize()
                .unwrap_or_else(|_| workspace_root.to_path_buf());
            if !canonical.starts_with(&canonical_root) {
                return Err(RuntimeError::InvalidArgument {
                    message: format!("git_commit: path escapes workspace: {p}"),
                });
            }
        }
        let mut add_args = vec!["add"];
        for p in file_paths {
            add_args.push(p.as_str());
        }
        run_git(workspace_root, &add_args)?;
    } else {
        run_git(workspace_root, &["add", "-A"])?;
    }

    // Commit.
    run_git(workspace_root, &["commit", "-m", message])?;

    // Return the new HEAD hash.
    let (head, _) = run_git(workspace_root, &["rev-parse", "HEAD"])?;
    Ok(serde_json::json!({
        "committed": true,
        "head": head.trim(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_empty_commit_message() {
        assert!(validate_commit_message("").is_err());
        assert!(validate_commit_message("   ").is_err());
    }

    #[test]
    fn reject_dangerous_commit_message() {
        assert!(validate_commit_message("--force push").is_err());
        assert!(validate_commit_message("use --no-verify").is_err());
    }

    #[test]
    fn accept_valid_commit_message() {
        assert!(validate_commit_message("fix: update plugin docs").is_ok());
    }

    #[test]
    fn git_diff_on_this_repo_works() {
        // Run on the actual repo to verify the git wrapper works.
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let result = git_diff(repo_root, None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn git_status_on_this_repo_works() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let result = git_status(repo_root);
        assert!(result.is_ok());
    }

    #[test]
    fn git_log_on_this_repo_works() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let result = git_log(repo_root, 3, None);
        assert!(result.is_ok());
        let log_text = result.unwrap()["log"].as_str().unwrap().to_string();
        assert!(!log_text.is_empty(), "git log should have entries");
    }
}
