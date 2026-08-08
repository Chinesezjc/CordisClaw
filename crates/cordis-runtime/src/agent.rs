use crate::config::LlmApiConfig;
use crate::core::error::RuntimeError;
use crate::host::RuntimeHost;
use chrono::Local;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared queue for Agent message injection.  The inbox thread pushes new
/// incoming messages here, and the Agent's respond loop drains them
/// between LLM turns so that late-arriving messages are seen.
///
/// P2-32 known limitation: this is a single process-wide queue. When more
/// than one session runs concurrently (e.g. `RuntimeShell` + a
/// `PluginIteration` agent driven in parallel from another thread), an
/// injected message lands in whichever session's `respond` loop happens
/// to drain first. Per-session queues would require the caller
/// (`main.rs::run_serve`) to route messages by session id — that
/// refactor is tracked separately; today's setup runs one primary
/// session at a time so the mis-routing hazard is latent, not active.
static AGENT_INJECT_QUEUE: Mutex<Option<Arc<Mutex<VecDeque<String>>>>> = Mutex::new(None);

/// Set the injection queue from the main binary.  Must be called once
/// before the Agent starts processing messages.
pub fn set_agent_inject_queue(q: Arc<Mutex<VecDeque<String>>>) {
    *AGENT_INJECT_QUEUE.lock().unwrap_or_else(|p| p.into_inner()) = Some(q);
}

/// Drain queued messages for injection into the current Agent turn.
fn drain_inject_queue() -> Vec<String> {
    let guard = AGENT_INJECT_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(ref q) = *guard {
        q.lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    } else {
        Vec::new()
    }
}

const AGENT_HISTORY_MESSAGE_LIMIT: usize = 8192;
const AGENT_MAX_TOOL_TURNS: usize = 96;
const UNKNOWN_TOOL_STRIKE_LIMIT: usize = 1;
const AGENT_TOOL_GET_RUNTIME_STATUS: &str = "get_runtime_status";
const AGENT_TOOL_LIST_PLUGINS: &str = "list_plugins";
const AGENT_TOOL_LIST_NODES: &str = "list_nodes";
const AGENT_TOOL_GET_KERNEL_STATUS: &str = "get_kernel_status";
const AGENT_TOOL_GET_KERNEL_ISSUES: &str = "get_kernel_issues";
const AGENT_TOOL_RELOAD_RUNTIME: &str = "reload_runtime";

/// Kernel introspection tools — their output describes the agent's own
/// internal state (plugin counts, snapshot ids, kernel counters), not
/// conversational content. Their tool call/result pairs are stripped by
/// [`filter_kernel_introspection_messages`] before persisting to cross-turn
/// history so they don't teach the LLM to imitate diagnostic call patterns on
/// later turns.
const KERNEL_INTROSPECTION_TOOLS: &[&str] = &[
    AGENT_TOOL_GET_RUNTIME_STATUS,
    AGENT_TOOL_LIST_PLUGINS,
    AGENT_TOOL_LIST_NODES,
    AGENT_TOOL_GET_KERNEL_STATUS,
    AGENT_TOOL_GET_KERNEL_ISSUES,
];

/// Returns true if `name` is one of the kernel introspection tools.
fn is_kernel_introspection(name: &str) -> bool {
    KERNEL_INTROSPECTION_TOOLS.contains(&name)
}

/// Strip kernel-introspection tool call/result pairs from a slice of chat
/// messages before they are persisted to cross-turn history.
///
/// Kernel introspection tools (see [`KERNEL_INTROSPECTION_TOOLS`]) report the
/// agent's own internal state rather than conversational content; persisting
/// their call/result pairs would teach the LLM to imitate diagnostic call
/// patterns on later turns. The tool_calls chain is kept valid throughout:
///
/// - An `assistant` message with no introspection `tool_calls` passes through
///   unchanged.
/// - For an `assistant` message that references introspection tools, every
///   introspection call id is recorded so its matching `tool` result is
///   dropped. The `tool_calls` array is shrunk to the non-introspection
///   subset. If nothing remains and the message has no textual content, the
///   whole message is dropped; if text remains, the `tool_calls` field is
///   removed so the surviving message keeps a valid chain.
/// - Any `tool` result message whose `tool_call_id` matches a dropped
///   introspection call is removed.
/// - All other messages pass through unchanged.
fn filter_kernel_introspection_messages(messages: &[Value]) -> Vec<Value> {
    let mut skip_tool_ids: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "assistant" {
            if let Some(tc_arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                if !tc_arr.is_empty() {
                    let (kernel_calls, other_calls): (Vec<&Value>, Vec<&Value>) =
                        tc_arr.iter().partition(|tc| {
                            tc.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .is_some_and(is_kernel_introspection)
                        });
                    if kernel_calls.is_empty() {
                        out.push(msg.clone());
                        continue;
                    }
                    // Record introspection call ids so their results drop.
                    for tc in &kernel_calls {
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            skip_tool_ids.push(id.to_string());
                        }
                    }
                    if other_calls.is_empty() {
                        // Purely introspection: keep only if textual content
                        // remains, and strip the now-dangling tool_calls.
                        let has_text = msg
                            .get("content")
                            .and_then(|v| v.as_str())
                            .is_some_and(|c| !c.trim().is_empty());
                        if !has_text {
                            continue;
                        }
                        let mut stripped = msg.clone();
                        if let Some(obj) = stripped.as_object_mut() {
                            obj.remove("tool_calls");
                        }
                        out.push(stripped);
                        continue;
                    }
                    // Mixed: keep only the non-introspection tool_calls.
                    let mut shrunk = msg.clone();
                    if let Some(obj) = shrunk.as_object_mut() {
                        obj.insert(
                            "tool_calls".to_string(),
                            Value::Array(other_calls.into_iter().cloned().collect()),
                        );
                    }
                    out.push(shrunk);
                    continue;
                }
            }
            out.push(msg.clone());
            continue;
        }
        if role == "tool" {
            if let Some(tid) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
                if skip_tool_ids.iter().any(|id| id == tid) {
                    continue;
                }
            }
        }
        out.push(msg.clone());
    }
    out
}
const AGENT_TOOL_BUILD_PLUGINS: &str = "build_plugins";
const AGENT_TOOL_INVOKE_PLUGIN: &str = "invoke_plugin";
const AGENT_TOOL_EXECUTE_TARGET: &str = "execute_target";
const AGENT_TOOL_READ_FILE: &str = "read_file";
const AGENT_TOOL_LIST_DIRECTORY: &str = "list_directory";
const AGENT_TOOL_SEARCH_CODE: &str = "search_code";
const AGENT_TOOL_WRITE_FILE: &str = "write_file";
const AGENT_TOOL_REPLACE_IN_FILE: &str = "replace_in_file";
const AGENT_TOOL_REVERT_CHANGES: &str = "revert_changes";
const AGENT_TOOL_DELETE_FILE: &str = "delete_file";
const AGENT_TOOL_RENAME_FILE: &str = "rename_file";
const AGENT_TOOL_MOVE_FILE: &str = "move_file";
const AGENT_TOOL_COPY_FILE: &str = "copy_file";
const AGENT_TOOL_COMPACT_CONTEXT: &str = "compact_context";
const AGENT_TOOL_RUN_PLUGIN_TEST: &str = "run_plugin_test";
const AGENT_TOOL_REQUEST_ITERATION: &str = "request_iteration";
const AGENT_TOOL_CREATE_PLUGIN: &str = "create_plugin";
const AGENT_TOOL_SET_SOUL: &str = "set_soul";
const LLM_DEBUG_ENV: &str = "CORDIS_LLM_DEBUG";

pub trait AgentToolHost {
    fn agent_runtime_status(&self) -> Result<Value, RuntimeError>;
    fn agent_list_plugins(&self) -> Result<Value, RuntimeError>;
    fn agent_list_nodes(&self) -> Result<Value, RuntimeError>;
    fn agent_kernel_status(&self) -> Result<Value, RuntimeError>;
    fn agent_kernel_issues(&self) -> Result<Value, RuntimeError>;
    fn agent_reload_runtime(&self, plugin_path: &str) -> Result<Value, RuntimeError>;
    fn agent_build_plugins(&self, plugin_name: &str) -> Result<Value, RuntimeError>;
    /// Collect system_hint strings from all loaded plugins. The Agent's
    /// system prompt should include these so that plugin-specific usage
    /// conventions (e.g. chat mode vs suspend for a messaging plugin) are
    /// injected automatically without hardcoding them in the kernel.
    fn agent_plugin_hints(&self) -> Vec<String>;
    fn agent_invoke_plugin(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload_json: Value,
    ) -> Result<Value, RuntimeError>;
    fn agent_execute_target(
        &self,
        node_fqn: &str,
        payload_json: Value,
    ) -> Result<Value, RuntimeError>;
    fn agent_read_file(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<Value, RuntimeError>;
    fn agent_list_directory(&self, path: &str) -> Result<Value, RuntimeError>;
    fn agent_search_code(&self, pattern: &str, path: Option<&str>) -> Result<Value, RuntimeError>;
    fn agent_write_file(&self, path: &str, content: &str) -> Result<Value, RuntimeError>;
    fn agent_replace_in_file(
        &self,
        path: &str,
        find: &str,
        replace: &str,
    ) -> Result<Value, RuntimeError>;
    fn agent_run_command(&self, command: &str) -> Result<Value, RuntimeError>;
    fn agent_revert_changes(&self) -> Result<Value, RuntimeError>;
    fn agent_delete_file(&self, path: &str) -> Result<Value, RuntimeError>;
    fn agent_rename_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError>;
    fn agent_move_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError>;
    fn agent_copy_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError>;
    fn agent_compact_context(&self, session_id: &str) -> Result<Value, RuntimeError>;
    fn agent_append_file(&self, path: &str, content: &str) -> Result<Value, RuntimeError>;
    fn agent_run_plugin_test(&self, command: Option<&str>) -> Result<Value, RuntimeError>;
    fn agent_request_iteration(
        &self,
        plugin_path: &str,
        instruction: &str,
    ) -> Result<Value, RuntimeError>;
    fn agent_create_plugin(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<Value, RuntimeError>;
    fn agent_send_warning_to_test_groups(&self, message: &str);
    /// O批: persona overlay text for the given soul scope, if any.
    /// Default None keeps non-runtime hosts (tests) working unchanged.
    fn agent_soul_overlay(&self, _soul_key: &str) -> Option<String> {
        None
    }
    /// O批: update the soul for a scope key. The KEY IS NOT LLM-SUPPLIED —
    /// callers must pass the current session's own soul_key so one user
    /// can never edit another user's persona.
    fn agent_set_soul(
        &self,
        _soul_key: &str,
        _persona: Option<&str>,
        _profile: Option<&str>,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidArgument {
            message: "soul storage is not available on this host".to_string(),
        })
    }
}

impl AgentToolHost for RuntimeHost {
    fn agent_runtime_status(&self) -> Result<Value, RuntimeError> {
        to_json_value("runtime status", self.status())
    }

    fn agent_list_plugins(&self) -> Result<Value, RuntimeError> {
        let snapshot = self.current_snapshot();
        let plugins = snapshot
            .plugin_registry()
            .iter()
            .map(|(plugin_path, plugin)| {
                json!({
                    "plugin_path": plugin_path,
                    "parent": plugin.parent,
                    "required": plugin.required,
                    "load_result": format!("{:?}", plugin.load_result),
                    "fingerprint_diff": plugin.fingerprint_diff,
                    "node_ids": plugin
                        .docs
                        .as_ref()
                        .map(|docs| docs.nodes.iter().map(|node| node.id.clone()).collect::<Vec<_>>())
                        .unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "snapshot_id": snapshot.snapshot_id(),
            "plugins": plugins,
        }))
    }

    fn agent_list_nodes(&self) -> Result<Value, RuntimeError> {
        let snapshot = self.current_snapshot();
        let nodes = snapshot
            .node_registry()
            .iter()
            .map(|(node_fqn, node)| {
                json!({
                    "node_fqn": node_fqn,
                    "plugin_path": node.plugin_path,
                    "node_id": node.node_id,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "snapshot_id": snapshot.snapshot_id(),
            "nodes": nodes,
        }))
    }

    fn agent_kernel_status(&self) -> Result<Value, RuntimeError> {
        to_json_value("kernel status", self.kernel().status())
    }

    fn agent_kernel_issues(&self) -> Result<Value, RuntimeError> {
        to_json_value("kernel issues", self.kernel().plugin_issues())
    }

    fn agent_reload_runtime(&self, plugin_path: &str) -> Result<Value, RuntimeError> {
        to_json_value(
            "reload diagnostics",
            self.reload_with_diagnostics(plugin_path),
        )
    }

    fn agent_build_plugins(&self, plugin_name: &str) -> Result<Value, RuntimeError> {
        use std::process::Command;
        let fixtures = self.fixtures_root();
        let manifest = fixtures.join("plugins").join("Cargo.toml");
        let mut cmd = Command::new("cargo");
        cmd.arg("build").arg("--manifest-path").arg(&manifest);
        if plugin_name != "all" {
            cmd.arg("-p").arg(plugin_name);
        }
        let output = cmd.output().map_err(|e| RuntimeError::InvalidArgument {
            message: format!("cargo build failed to start: {e}"),
        })?;
        let ok = output.status.success();
        let mut result = json!({
            "ok": ok,
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
        });
        // Sync built .so to artifacts/ via a staging copy, then atomic reload.
        // Writing directly over the loaded .so causes SIGSEGV; writing to a
        // staging name and then atomically swapping via reload is safe.
        if ok && plugin_name != "all" {
            let target_dir = fixtures.join("plugins").join("target").join("debug");
            let src = target_dir.join(format!("lib{}.so", plugin_name.replace('-', "_")));
            let artifacts_dir = fixtures.join("artifacts");
            let _ = std::fs::create_dir_all(&artifacts_dir);
            let staging = artifacts_dir.join(format!(".{}.staging.so", plugin_name));
            let dst = artifacts_dir.join(format!("{}.so", plugin_name));
            // Copy to staging first so the live .so is never overwritten in-place.
            match std::fs::copy(&src, &staging) {
                Ok(bytes) => {
                    // Atomically rename staging → live (same filesystem = atomic).
                    let _ = std::fs::rename(&staging, &dst);
                    result["synced_artifact"] = json!(format!(
                        "{} -> artifacts/{}.so ({} bytes)",
                        src.display(),
                        plugin_name,
                        bytes
                    ));
                    eprintln!(
                        "build_plugins: synced {} -> {}",
                        src.display(),
                        dst.display()
                    );
                    // Reload: old snapshot is dropped, new snapshot loads the new .so.
                    match self.agent_reload_runtime(&format!("/{plugin_name}")) {
                        Ok(reload) => {
                            result["reload"] = reload;
                        }
                        Err(e) => {
                            eprintln!("build_plugins: reload failed for {plugin_name}: {e}");
                            return Err(e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("build_plugins: artifact sync skipped for {plugin_name}: {e}");
                    result["synced_artifact"] = json!(null);
                }
            }
        }
        Ok(result)
    }

    fn agent_plugin_hints(&self) -> Vec<String> {
        let snapshot = self.current_snapshot();
        // Build a concise catalog of available plugins and their nodes.
        let mut catalog = String::from("Loaded plugins:\n");
        for (path, plugin) in snapshot.plugin_registry().iter() {
            if let Some(ref docs) = plugin.docs {
                let agent_nodes: Vec<&str> = docs
                    .nodes
                    .iter()
                    .filter(|n| n.agent_accessible)
                    .map(|n| n.id.as_str())
                    .collect();
                if !agent_nodes.is_empty() {
                    catalog.push_str(&format!("  {} → {}\n", path, agent_nodes.join(", ")));
                }
            }
        }
        let mut hints = vec![catalog];
        hints.extend(
            snapshot
                .plugin_registry()
                .iter()
                .filter_map(|(_, plugin)| plugin.docs)
                .filter_map(|docs| docs.system_hint),
        );
        hints
    }

    fn agent_invoke_plugin(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload_json: Value,
    ) -> Result<Value, RuntimeError> {
        self.check_agent_accessible(plugin_path, node_id)?;
        let payload_text =
            serde_json::to_string(&payload_json).map_err(|err| RuntimeError::Invariant {
                message: format!("failed to serialize invoke payload for agent tool: {err}"),
            })?;
        let response = self.invoke(plugin_path, node_id, payload_text)?;
        Ok(json!({
            "plugin_path": plugin_path,
            "node_id": node_id,
            "payload": parse_json_or_string(&response.payload),
        }))
    }

    fn agent_execute_target(
        &self,
        node_fqn: &str,
        payload_json: Value,
    ) -> Result<Value, RuntimeError> {
        if let Some((plugin_path, node_id)) = node_fqn.split_once("::") {
            self.check_agent_accessible(plugin_path, node_id)?;
        }
        let response = self.execute(node_fqn, payload_json)?;
        to_json_value("execution result", response)
    }

    fn agent_read_file(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<Value, RuntimeError> {
        self.check_sensitive_path(path)?;
        let resolved = self.resolve_sandboxed_path(path)?;
        // P1-28: cap the amount of data we load into memory before slicing
        // by offset/limit. `read_to_string` on a multi-GB file (a plugin
        // log, an accidental `/dev/zero` bypass, etc.) would blow the
        // process. Reject anything above 16 MiB up front — big enough for
        // any legitimate source file, small enough to bound memory.
        const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;
        if let Ok(meta) = std::fs::metadata(&resolved) {
            if meta.len() > MAX_READ_BYTES {
                return Err(RuntimeError::InvalidArgument {
                    message: format!(
                        "read_file: file {path} exceeds {} MiB limit (size={} bytes)",
                        MAX_READ_BYTES / (1024 * 1024),
                        meta.len()
                    ),
                });
            }
        }
        let content = std::fs::read_to_string(&resolved).map_err(|err| RuntimeError::Io {
            path: resolved,
            message: err.to_string(),
        })?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = offset.unwrap_or(0).min(total);
        let end = limit.map(|n| (start + n).min(total)).unwrap_or(total);
        let excerpt: Vec<serde_json::Value> = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| json!({"line": start + i + 1, "text": line}))
            .collect();
        Ok(json!({
            "path": path,
            "total_lines": total,
            "offset": start,
            "limit": end - start,
            "lines": excerpt,
        }))
    }

    fn agent_list_directory(&self, path: &str) -> Result<Value, RuntimeError> {
        self.check_sensitive_path(path)?;
        let resolved = self.resolve_sandboxed_path(path)?;
        let mut entries = Vec::new();
        if resolved.is_dir() {
            for entry in std::fs::read_dir(&resolved).map_err(|err| RuntimeError::Io {
                path: resolved.clone(),
                message: err.to_string(),
            })? {
                let entry = entry.map_err(|err| RuntimeError::Io {
                    path: resolved.clone(),
                    message: err.to_string(),
                })?;
                let ft = entry.file_type().map_err(|err| RuntimeError::Io {
                    path: entry.path(),
                    message: err.to_string(),
                })?;
                entries.push(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "kind": if ft.is_dir() { "dir" } else { "file" },
                }));
            }
        }
        entries.sort_by(|a, b| {
            let kind_cmp = a["kind"].as_str().cmp(&b["kind"].as_str());
            if kind_cmp == std::cmp::Ordering::Equal {
                a["name"].as_str().cmp(&b["name"].as_str())
            } else {
                kind_cmp
            }
        });
        Ok(json!({
            "path": path,
            "entries": entries,
        }))
    }

    fn agent_search_code(&self, pattern: &str, path: Option<&str>) -> Result<Value, RuntimeError> {
        if let Some(p) = path {
            self.check_sensitive_path(p)?;
        }
        let search_root = match path {
            Some(p) => self.resolve_sandboxed_path(p)?,
            None => self.fixtures_root().to_path_buf(),
        };
        let mut matches = Vec::new();
        let mut walked = 0usize;
        // P1-27: stop the walker as soon as we have 40 hits. The old
        // implementation only `break`'d the inner line loop, so
        // `walk_code_files` kept traversing the whole tree and
        // `read_to_string`ing every candidate file even though the caller
        // was capped at 40 rows.
        const MAX_HITS: usize = 40;
        self.walk_code_files_ctl(&search_root, &mut |rel_path, abs_path| {
            walked += 1;
            let content = match std::fs::read_to_string(abs_path) {
                Ok(c) => c,
                Err(_) => return crate::host::WalkControl::Continue,
            };
            for (line_no, line_text) in content.lines().enumerate() {
                if line_text.contains(pattern) {
                    matches.push(json!({
                        "path": rel_path,
                        "line": line_no + 1,
                        "text": line_text.trim(),
                    }));
                    if matches.len() >= MAX_HITS {
                        break;
                    }
                }
            }
            if matches.len() >= MAX_HITS {
                crate::host::WalkControl::Stop
            } else {
                crate::host::WalkControl::Continue
            }
        })?;
        Ok(json!({
            "pattern": pattern,
            "search_root": search_root.strip_prefix(self.fixtures_root()).unwrap_or(&search_root).to_string_lossy(),
            "files_walked": walked,
            "matches": matches,
        }))
    }

    fn agent_write_file(&self, path: &str, content: &str) -> Result<Value, RuntimeError> {
        self.check_sensitive_path(path)?;
        let resolved = self.resolve_sandboxed_path(path)?;
        // Backup original before writing.
        let original = std::fs::read(&resolved).ok();
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
                path: parent.to_path_buf(),
                message: err.to_string(),
            })?;
        }
        std::fs::write(&resolved, content).map_err(|err| RuntimeError::Io {
            path: resolved,
            message: err.to_string(),
        })?;
        // Accumulate rollback backup.
        {
            let mut rollback = self.interactive_rollback();
            let backup = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
                self.fixtures_root(),
                path,
                original,
            );
            rollback.absorb(backup)?;
        }
        Ok(json!({
            "path": path,
            "written_bytes": content.len(),
        }))
    }

    fn agent_replace_in_file(
        &self,
        path: &str,
        find: &str,
        replace: &str,
    ) -> Result<Value, RuntimeError> {
        let resolved = self.resolve_sandboxed_path(path)?;
        let original = std::fs::read_to_string(&resolved).map_err(|err| RuntimeError::Io {
            path: resolved.clone(),
            message: err.to_string(),
        })?;
        // P1-29: require `find` to appear exactly once. `replacen(..., 1)`
        // was silently mis-targeting the first occurrence when the
        // agent-generated `find` string appeared multiple times in the
        // file (common for short identifiers). Force the agent to supply
        // enough surrounding context to disambiguate, or use a different
        // tool for multi-replace.
        let occurrences = original.matches(find).count();
        if occurrences == 0 {
            return Err(RuntimeError::InvalidArgument {
                message: format!("replace_in_file: pattern not found in {path}: {find}"),
            });
        }
        if occurrences > 1 {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "replace_in_file: pattern appears {occurrences} times in {path}; \
                     provide more surrounding context so the target is unique"
                ),
            });
        }
        let updated = original.replacen(find, replace, 1);
        // Backup original bytes before writing.
        let original_bytes = Some(original.into_bytes());
        std::fs::write(&resolved, &updated).map_err(|err| RuntimeError::Io {
            path: resolved.clone(),
            message: err.to_string(),
        })?;
        {
            let mut rollback = self.interactive_rollback();
            let backup = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
                self.fixtures_root(),
                path,
                original_bytes,
            );
            rollback.absorb(backup)?;
        }
        Ok(json!({
            "path": path,
            "replaced": true,
            "occurrences": 1,
        }))
    }

    fn agent_run_command(&self, command: &str) -> Result<Value, RuntimeError> {
        self.check_sensitive_command(command)?;
        // P0-17: previously `sh -c command` — the whole string went through a
        // shell, so `; rm -rf ~`, `` `curl x|sh` ``, `$(...)`, redirections
        // and pipes all executed. Tokenise via shell_words (POSIX quoting,
        // no expansion) and dispatch argv[0] directly with `Command::new`.
        // Anything shell-meta-y ends up as a literal argv element that the
        // target program either doesn't recognise or treats as data.
        use std::process::Command;
        let argv = shell_words::split(command).map_err(|err| RuntimeError::InvalidArgument {
            message: format!("run_command tokenisation failed: {err}"),
        })?;
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| RuntimeError::InvalidArgument {
                message: "run_command received an empty command string".to_string(),
            })?;
        if program.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "run_command program was empty after tokenisation".to_string(),
            });
        }
        let output = Command::new(program)
            .args(args)
            .current_dir(self.fixtures_root())
            .output()
            .map_err(|err| RuntimeError::Io {
                path: self.fixtures_root().to_path_buf(),
                message: format!("failed to run command: {err}"),
            })?;
        Ok(json!({
            "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
            "exit_code": output.status.code(),
        }))
    }

    fn agent_revert_changes(&self) -> Result<Value, RuntimeError> {
        let mut rollback = self.interactive_rollback();
        let count = rollback.len();
        rollback.rollback()?;
        *rollback =
            crate::kernel::plugin_iteration::PluginEditRollback::empty(self.fixtures_root());
        Ok(json!({
            "reverted_files": count,
        }))
    }

    fn agent_delete_file(&self, path: &str) -> Result<Value, RuntimeError> {
        self.check_sensitive_path(path)?;
        let resolved = self.resolve_sandboxed_path(path)?;
        if resolved.is_dir() {
            return Err(RuntimeError::InvalidArgument {
                message: format!("delete_file: path is a directory, not a file: {path}"),
            });
        }
        let original = std::fs::read(&resolved).map_err(|err| RuntimeError::Io {
            path: resolved.clone(),
            message: err.to_string(),
        })?;
        std::fs::remove_file(&resolved).map_err(|err| RuntimeError::Io {
            path: resolved.clone(),
            message: err.to_string(),
        })?;
        let mut rollback = self.interactive_rollback();
        let backup = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
            self.fixtures_root(),
            path,
            Some(original),
        );
        rollback.absorb(backup)?;
        Ok(json!({
            "path": path,
            "deleted": true,
        }))
    }

    fn agent_rename_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError> {
        self.check_sensitive_path(path)?;
        self.check_sensitive_path(new_path)?;
        let resolved_src = self.resolve_sandboxed_path(path)?;
        let resolved_dst = self.resolve_sandboxed_path(new_path)?;
        let original = std::fs::read(&resolved_src).ok();
        // P1-30: back up the destination if it already exists — otherwise
        // `rename` overwrites the destination content and `revert_changes`
        // is unable to restore it.
        let dst_original = std::fs::read(&resolved_dst).ok();
        if let Some(parent) = resolved_dst.parent() {
            std::fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
                path: parent.to_path_buf(),
                message: err.to_string(),
            })?;
        }
        std::fs::rename(&resolved_src, &resolved_dst).map_err(|err| RuntimeError::Io {
            path: resolved_src.clone(),
            message: err.to_string(),
        })?;
        let mut rollback = self.interactive_rollback();
        let backup = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
            self.fixtures_root(),
            path,
            original,
        );
        rollback.absorb(backup)?;
        if let Some(dst_bytes) = dst_original {
            let dst_backup = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
                self.fixtures_root(),
                new_path,
                Some(dst_bytes),
            );
            rollback.absorb(dst_backup)?;
        }
        Ok(json!({
            "path": path,
            "new_path": new_path,
            "renamed": true,
        }))
    }

    fn agent_move_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError> {
        self.agent_rename_file(path, new_path)
    }

    fn agent_copy_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError> {
        self.check_sensitive_path(path)?;
        self.check_sensitive_path(new_path)?;
        let resolved_src = self.resolve_sandboxed_path(path)?;
        let resolved_dst = self.resolve_sandboxed_path(new_path)?;
        // Backup destination original if it exists (for rollback).
        let dst_original = std::fs::read(&resolved_dst).ok();
        if let Some(parent) = resolved_dst.parent() {
            std::fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
                path: parent.to_path_buf(),
                message: err.to_string(),
            })?;
        }
        std::fs::copy(&resolved_src, &resolved_dst).map_err(|err| RuntimeError::Io {
            path: resolved_src.clone(),
            message: err.to_string(),
        })?;
        let mut rollback = self.interactive_rollback();
        // Back up destination at new_path (the file we just overwrote or created).
        let backup = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
            self.fixtures_root(),
            new_path,
            dst_original,
        );
        rollback.absorb(backup)?;
        Ok(json!({
            "path": path,
            "new_path": new_path,
            "copied": true,
        }))
    }

    fn agent_append_file(&self, path: &str, content: &str) -> Result<Value, RuntimeError> {
        self.check_sensitive_path(path)?;
        let resolved = self.resolve_sandboxed_path(path)?;
        // Backup original before appending.
        let original = std::fs::read(&resolved).ok();
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
                path: parent.to_path_buf(),
                message: err.to_string(),
            })?;
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&resolved)
            .map_err(|err| RuntimeError::Io {
                path: resolved.clone(),
                message: err.to_string(),
            })?;
        file.write_all(content.as_bytes())
            .map_err(|err| RuntimeError::Io {
                path: resolved,
                message: err.to_string(),
            })?;
        let mut rollback = self.interactive_rollback();
        let backup = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
            self.fixtures_root(),
            path,
            original,
        );
        rollback.absorb(backup)?;
        Ok(json!({
            "path": path,
            "appended_bytes": content.len(),
        }))
    }

    fn agent_compact_context(&self, session_id: &str) -> Result<Value, RuntimeError> {
        // P1-25: during `agent_send` the session is removed from the map
        // for the duration of `respond`, so this lookup misses when the
        // agent calls compact on itself mid-turn. Detect that case and
        // queue a `PendingSessionAction::CompactHistory` on the host —
        // it drains and applies before reinserting the session after the
        // turn ends. Returns `{"deferred": true}` so the LLM knows the
        // action is scheduled rather than failed.
        {
            let mut guard = self.agent_sessions_mut();
            if let Some(session) = guard.get_mut(session_id) {
                let (old_len, new_len) = session.compact_history();
                return Ok(json!({
                    "compacted": true,
                    "old_messages": old_len,
                    "new_messages": new_len,
                }));
            }
        }
        // Session is currently executing a turn — queue for post-turn.
        self.queue_session_action(
            session_id,
            crate::host::PendingSessionAction::CompactHistory,
        );
        Ok(json!({
            "compacted": false,
            "deferred": true,
            "reason": "session is currently mid-turn; compact will run after this turn completes",
        }))
    }

    fn agent_run_plugin_test(&self, command: Option<&str>) -> Result<Value, RuntimeError> {
        let cmd = command
            .map(|c| c.to_string())
            .unwrap_or_else(|| "cargo test --quiet --manifest-path plugins/Cargo.toml".to_string());
        self.check_sensitive_command(&cmd)?;
        // P0-17: previously `bash -lc <cmd>` — the whole string went through a
        // shell, so `; rm -rf ~`, `$(...)`, pipes and redirections all executed.
        // Tokenise via shell_words (POSIX quoting, no expansion) and dispatch
        // argv[0] directly, mirroring agent_run_command: shell-meta fragments
        // survive as literal argv elements that the target program either
        // rejects or treats as data. (A command that is *itself* an interpreter
        // invocation, e.g. `bash -lc ...`, can still reach that interpreter via
        // argv — the sensitive-command gate is the kernel's guard for that.)
        use std::process::Command;
        let argv = shell_words::split(&cmd).map_err(|err| RuntimeError::InvalidArgument {
            message: format!("run_plugin_test tokenisation failed: {err}"),
        })?;
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| RuntimeError::InvalidArgument {
                message: "run_plugin_test received an empty command string".to_string(),
            })?;
        if program.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "run_plugin_test program was empty after tokenisation".to_string(),
            });
        }
        let output = Command::new(program)
            .args(args)
            .current_dir(self.fixtures_root())
            .output()
            .map_err(|err| RuntimeError::CommandFailed {
                program: program.to_string(),
                args: args.to_vec(),
                message: err.to_string(),
            })?;
        Ok(json!({
            "command": cmd,
            "success": output.status.success(),
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
        }))
    }

    fn agent_request_iteration(
        &self,
        plugin_path: &str,
        instruction: &str,
    ) -> Result<Value, RuntimeError> {
        let pp = plugin_path.trim_start_matches('/');
        // Empty plugin_path (from "/") means root workspace mode — the agent
        // can create/edit files anywhere under plugins/, not just one subtree.
        let target_plugin_paths: Vec<String> = if pp.is_empty() {
            vec![]
        } else {
            vec![pp.to_string()]
        };
        let request = crate::kernel::plugin_iteration::KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths,
            instruction: Some(instruction.to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: None,
            quality_score: None,
        };
        match self.iterate_plugins(request) {
            Ok(result) => Ok(serde_json::json!({
                "ok": true,
                "summary": result.summary,
                "verdict": format!("{:?}", result.verifier_verdict),
            })),
            Err(e) => Ok(serde_json::json!({
                "ok": false,
                "error": e.to_string(),
            })),
        }
    }

    fn agent_create_plugin(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<Value, RuntimeError> {
        self.create_plugin(name, description)
    }

    fn agent_send_warning_to_test_groups(&self, message: &str) {
        crate::kernel::notify::send(self, message);
    }

    fn agent_soul_overlay(&self, soul_key: &str) -> Option<String> {
        match self.get_soul(soul_key) {
            Ok(Some(soul)) if !soul.persona.trim().is_empty() => Some(soul.persona),
            Ok(_) => None,
            Err(err) => {
                eprintln!("[soul] overlay lookup failed for {soul_key}: {err}");
                None
            }
        }
    }

    fn agent_set_soul(
        &self,
        soul_key: &str,
        persona: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Value, RuntimeError> {
        if soul_key.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "this session has no user identity; soul editing requires a \
                          channel session (Feishu/QQ) with sender info"
                    .to_string(),
            });
        }
        // Merge onto the existing record so setting only the profile
        // doesn't wipe the persona (and vice versa).
        let mut soul = self.get_soul(soul_key)?.unwrap_or_default();
        if let Some(p) = persona {
            soul.persona = p.to_string();
        }
        if let Some(p) = profile {
            let trimmed = p.trim();
            soul.profile = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            if let Some(name) = &soul.profile {
                let profiles = &self.config().llm_profiles.profiles;
                if !profiles.contains_key(name) {
                    return Err(RuntimeError::InvalidArgument {
                        message: format!(
                            "unknown LLM profile '{name}'; available: {}",
                            profiles.keys().cloned().collect::<Vec<_>>().join(", ")
                        ),
                    });
                }
            }
        }
        soul.updated_at_ms = crate::kernel::plugin_iteration::now_ms() as u64;
        soul.updated_by = "agent".to_string();
        self.set_soul(soul_key, &soul)?;
        Ok(json!({
            "ok": true,
            "soul_key": soul_key,
            "persona_chars": soul.persona.chars().count(),
            "profile": soul.profile.clone().unwrap_or_else(|| "default".to_string()),
            "note": "changes apply to NEW sessions (after /reset); the current conversation keeps its prompt",
        }))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentSessionStatus {
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub completed_turns: usize,
    pub stored_messages: usize,
    pub transcript_events: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentReply {
    pub response_id: Option<String>,
    pub content: String,
    pub tool_events: Vec<AgentToolEvent>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentToolEvent {
    pub name: String,
    pub arguments: Value,
    pub ok: bool,
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentTranscriptEntry {
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default)]
        response_id: Option<String>,
    },
    Tool {
        name: String,
        arguments: Value,
        ok: bool,
        #[serde(default)]
        output: Option<Value>,
        #[serde(default)]
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentToolExecutionSummary {
    pub total_calls: usize,
    pub successful_calls: usize,
    pub failed_calls: usize,
    pub tool_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AgentToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

pub trait AgentBackend {
    type Host: AgentToolHost + ?Sized;
    fn system_prompt(&self) -> String;
    fn tool_specs(&self) -> Vec<AgentToolSpec>;
    fn execute_tool(&mut self, name: &str, arguments: Value) -> Result<Value, RuntimeError>;
    fn host(&self) -> &Self::Host;
    fn terminal_tool_reply(&self, _name: &str, _output: &Value) -> Option<String> {
        None
    }
    fn tool_scope_label(&self) -> String {
        "agent".to_string()
    }
}

/// 一次模型补全的执行者。
///
/// 这是 agent 循环（机制）与供应商传输（可覆写）之间的接缝。循环只要求拿回
/// **结构化的** `tool_calls`——它在本轮中途就要据此分派工具，因此只回文本的
/// provider 无法驱动工具调用。
pub(crate) trait LlmProvider {
    /// `sink` 为 `Some` 时 provider **可以**逐段推送增量；忽略它也必须正确工作，
    /// 权威结果始终是本函数的返回值。
    fn complete(
        &self,
        body: Value,
        sink: Option<std::sync::Arc<dyn crate::llm_sink::TokenSink>>,
        transport: cordis_plugin_sdk::llm::LlmTransportConfig,
    ) -> Result<LlmCompletionParts, RuntimeError>;
}

/// 一次补全的返回三件套。具名结构体而非裸元组：它要跨 `agent`/`host` 两个模块
/// 使用，字段名比位置更经得起演进。
#[derive(Debug)]
pub(crate) struct LlmCompletionParts {
    pub(crate) message: cordis_plugin_sdk::llm::LlmMessage,
    pub(crate) response_id: Option<String>,
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentSession {
    kind: String,
    config: LlmApiConfig,
    history: Vec<Value>,
    transcript: Vec<AgentTranscriptEntry>,
    completed_turns: usize,
    /// Conservative estimate of total tokens in history (chars / 2).
    estimated_tokens: usize,
    /// Consecutive reasoning-only responses (no content, no tool_calls).
    reasoning_only_strikes: usize,
    /// Consecutive calls to tools that don't exist in this session (LLM hallucination guard).
    unknown_tool_strikes: usize,
    /// O批: soul scope key ({sender_id}#{conversation_kind}) this session
    /// serves. Empty = no per-user soul (REPL, legacy callers).
    soul_key: String,
}

/// Serializable snapshot of an AgentSession for crash recovery.
/// Captures everything needed to reconstruct the session except the
/// LLM 传输（已移出 kernel，见 fixtures/plugins/llm_openai）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionSnapshot {
    pub kind: String,
    pub config: LlmApiConfig,
    pub history: Vec<Value>,
    pub transcript: Vec<AgentTranscriptEntry>,
    pub completed_turns: usize,
    pub estimated_tokens: usize,
    pub reasoning_only_strikes: usize,
    pub unknown_tool_strikes: usize,
    /// `#[serde(default)]` keeps pre-O批 snapshots deserializable.
    #[serde(default)]
    pub soul_key: String,
}

pub type ShellAgentStatus = AgentSessionStatus;
pub type ShellAgentReply = AgentReply;

#[derive(Debug, Clone)]
pub struct ShellAgentSession {
    inner: AgentSession,
}

/// Conservative token estimate: 1 token per 2 characters (assumes mostly
/// Chinese text which is more token-dense than English).
fn estimate_tokens(s: &str) -> usize {
    s.chars().count() / 2
}

impl AgentSession {
    /// 仍返回 `Result` 以保持调用方签名不变：拆分前这里会构造 reqwest client
    /// 因而可能失败，现在传输在插件里、本函数不再有失败路径，但把 ~20 处调用
    /// 点一起改签名不属于本批范围。
    pub fn new(config: LlmApiConfig, kind: impl Into<String>) -> Result<Self, RuntimeError> {
        Ok(Self {
            kind: kind.into(),
            config,
            history: Vec::new(),
            transcript: Vec::new(),
            completed_turns: 0,
            estimated_tokens: 0,
            reasoning_only_strikes: 0,
            unknown_tool_strikes: 0,
            soul_key: String::new(),
        })
    }

    /// O批: bind this session to a soul scope key. Set once at
    /// agent_start; the backend reads it to fetch the persona overlay.
    pub fn set_soul_key(&mut self, soul_key: impl Into<String>) {
        self.soul_key = soul_key.into();
    }

    pub fn soul_key(&self) -> &str {
        &self.soul_key
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.transcript.clear();
        self.completed_turns = 0;
    }

    /// Swap the LLM config (profile fallback switching). history/transcript
    /// are untouched so the conversation continues seamlessly on the other
    /// endpoint.
    ///
    /// 拆分前这里要重建 reqwest client（新的 timeout），因而有一条失败臂；
    /// 现在 timeout 随 `to_transport()` 投影在每次调用时传给插件，本函数只是
    /// 换个配置结构体。`Result` 保留是为了不动 fallback 链路的调用点。
    pub fn swap_config(&mut self, config: LlmApiConfig) -> Result<(), RuntimeError> {
        self.config = config;
        Ok(())
    }

    pub fn status(&self) -> AgentSessionStatus {
        AgentSessionStatus {
            kind: self.kind.clone(),
            provider: self.config.provider.clone(),
            model: self.config.model.clone(),
            completed_turns: self.completed_turns,
            stored_messages: self.history.len(),
            transcript_events: self.transcript.len(),
        }
    }

    /// P1-24(b): expose the session's own kind so crash-recovery can
    /// re-wire it under the correct `AgentSessionKind` instead of
    /// hardcoding RuntimeShell.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn transcript(&self) -> &[AgentTranscriptEntry] {
        &self.transcript
    }

    pub fn tool_execution_summary(&self) -> AgentToolExecutionSummary {
        let mut tool_names = BTreeSet::new();
        let mut total_calls = 0usize;
        let mut successful_calls = 0usize;
        let mut failed_calls = 0usize;
        for entry in &self.transcript {
            let AgentTranscriptEntry::Tool { name, ok, .. } = entry else {
                continue;
            };
            total_calls += 1;
            if *ok {
                successful_calls += 1;
            } else {
                failed_calls += 1;
            }
            tool_names.insert(name.clone());
        }
        AgentToolExecutionSummary {
            total_calls,
            successful_calls,
            failed_calls,
            tool_names: tool_names.into_iter().collect(),
        }
    }

    /// 无 provider 可用时的入口：直接报 [`RuntimeError::NoLlmProvider`]。
    ///
    /// LLM 传输已整体移出 kernel（见 `fixtures/plugins/llm_openai`），没有内建
    /// 兜底。保留本函数是为了让"没装 provider 插件"这件事在调用点就有明确
    /// 报错，而不是让每个调用方各自处理 `Option<provider>`。
    ///
    /// 冷启动不受影响：boot / REPL / `command_router` 都不经这里，无插件时
    /// `/status`、`/help` 等照常工作，只有真的要跟模型说话才会失败——与拆分前
    /// "LLM 不可达"的状态同构。
    pub fn respond<B: AgentBackend + ?Sized>(
        &mut self,
        _backend: &mut B,
        _user_input: &str,
    ) -> Result<AgentReply, RuntimeError> {
        Err(RuntimeError::NoLlmProvider)
    }

    /// P2-31: 出错时若只推了 User 而没有对应的 Assistant，补一条描述错误的
    /// Assistant 占位。否则同一 session id 重试会看到孤立的 User，模型下一轮
    /// 会把提示词重复记一遍。
    fn repair_transcript_on_error(
        &mut self,
        transcript_len_before: usize,
        result: &Result<AgentReply, RuntimeError>,
    ) {
        let Err(err) = result else { return };
        let has_user_without_assistant = self
            .transcript
            .iter()
            .skip(transcript_len_before)
            .any(|e| matches!(e, AgentTranscriptEntry::User { .. }))
            && !self
                .transcript
                .iter()
                .skip(transcript_len_before)
                .any(|e| matches!(e, AgentTranscriptEntry::Assistant { .. }));
        if has_user_without_assistant {
            self.transcript.push(AgentTranscriptEntry::Assistant {
                content: format!("[error] {err}"),
                response_id: None,
            });
        }
    }

    /// `provider` 承担这一轮所有的模型调用；`sink` 决定增量 token 去哪。
    ///
    /// 两者都作为参数传入而非存进 `AgentSession`：该结构体 derive 了
    /// `Debug + Clone` 并会被快照序列化，塞 `dyn Trait` 会同时破坏这三者。
    /// 这也与既有的 `backend` 参数保持一致。
    fn respond_inner<B: AgentBackend + ?Sized>(
        &mut self,
        backend: &mut B,
        user_input: &str,
        provider: &dyn LlmProvider,
        sink: Option<std::sync::Arc<dyn crate::llm_sink::TokenSink>>,
    ) -> Result<AgentReply, RuntimeError> {
        let trimmed = user_input.trim();
        if trimmed.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "agent input must not be empty".to_string(),
            });
        }

        // 供应商白名单闸门。注意它只是个字符串校验，全仓没有一处按它分支
        // wire format——C4 接入 provider 解析后本段整体移除。
        let configured_provider = self.config.provider.trim().to_ascii_lowercase();
        if configured_provider != "deepseek" && configured_provider != "openai" {
            return Err(RuntimeError::UnsupportedLlmProvider {
                provider: self.config.provider.clone(),
            });
        }

        // If this message would push us over the threshold, compact first.
        if self.estimated_tokens + estimate_tokens(trimmed) > 800_000 {
            let _ = self.compact_history();
        }

        let mut messages = Vec::with_capacity(self.history.len() + 3);
        messages.push(json!({
            "role": "system",
            "content": backend.system_prompt(),
        }));
        messages.extend(self.history.clone());
        messages.push(json!({
            "role": "user",
            "content": trimmed,
        }));
        self.transcript.push(AgentTranscriptEntry::User {
            content: trimmed.to_string(),
        });

        let turn_started = Instant::now();
        let mut tool_events = Vec::new();

        for turn in 0..AGENT_MAX_TOOL_TURNS {
            // P1-37: unify the two timeout budgets. `timeout_ms` (per
            // HTTP request) and `stream_timeout_secs * 5` (per-stream
            // overall) were checked independently — a session where
            // `stream_timeout_secs` was set generously would keep
            // running past what `timeout_ms` implied and vice versa.
            // The effective budget here is
            //   max(timeout_ms, stream_timeout_secs * 1000 * 5)
            // × AGENT_MAX_TOOL_TURNS, but since turns share the wall
            // clock we cap at the larger of the two; the outer caller
            // still bounds via AGENT_MAX_TOOL_TURNS itself.
            let per_request_ms = self.config.timeout_ms;
            let stream_budget_ms = self.config.stream_timeout_secs.saturating_mul(1000 * 5);
            let effective_budget_ms = per_request_ms.max(stream_budget_ms);
            if turn_started.elapsed() >= Duration::from_millis(effective_budget_ms) {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!(
                        "agent exceeded total response budget after {} tool turns; elapsed_ms={} effective_budget_ms={} (timeout_ms={} stream_secs={})",
                        turn,
                        turn_started.elapsed().as_millis(),
                        effective_budget_ms,
                        self.config.timeout_ms,
                        self.config.stream_timeout_secs,
                    ),
                });
            }

            // Check for new messages since last turn and inject them.
            {
                let new_msgs = drain_inject_queue();
                for m in new_msgs {
                    if m.trim().is_empty() {
                        continue;
                    }
                    emit_agent_diagnostic(format!(
                        "agent_inject kind={} turn={} len={}",
                        self.kind,
                        turn + 1,
                        m.len()
                    ));
                    messages.push(json!({"role": "user", "content": &m}));
                    self.transcript
                        .push(AgentTranscriptEntry::User { content: m });
                }
            }

            let tool_specs = backend.tool_specs();
            emit_agent_diagnostic(format!(
                "agent_turn_start kind={} turn={} elapsed_ms={} messages={} tools={}",
                self.kind,
                turn + 1,
                turn_started.elapsed().as_millis(),
                messages.len(),
                tool_specs.len(),
            ));

            // Repair broken tool_calls chains: if an assistant message has
            // tool_calls without matching tool results, filter the tool_calls
            // array so the remaining chain is valid for DeepSeek.
            let mut repair_indices: Vec<(usize, Vec<Value>)> = Vec::new();
            for (idx, msg) in messages.iter().enumerate() {
                if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                    if let Some(tc_arr) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                        let expected: Vec<&str> = tc_arr
                            .iter()
                            .filter_map(|tc| tc.get("id").and_then(|v| v.as_str()))
                            .collect();
                        let mut found: Vec<&str> = Vec::new();
                        for next in messages.iter().skip(idx + 1) {
                            match next.get("role").and_then(|v| v.as_str()) {
                                Some("tool") => {
                                    if let Some(tid) =
                                        next.get("tool_call_id").and_then(|v| v.as_str())
                                    {
                                        found.push(tid);
                                    }
                                }
                                Some("assistant") | Some("user") => break,
                                _ => {}
                            }
                        }
                        if found.len() < expected.len() {
                            eprintln!(
                                "[tool_calls repair] turn={} msg_idx={} expected={:?} found={:?}",
                                turn + 1,
                                idx,
                                expected,
                                found,
                            );
                            // Keep only tool_calls that have matching tool messages.
                            let repaired: Vec<Value> = tc_arr
                                .iter()
                                .filter(|tc| {
                                    tc.get("id")
                                        .and_then(|v| v.as_str())
                                        .is_some_and(|id| found.contains(&id))
                                })
                                .cloned()
                                .collect();
                            if repaired.is_empty() {
                                // No tool calls have results — remove the entire
                                // assistant message from the array (we'll do this
                                // after the scan).
                                repair_indices.push((idx, repaired));
                            } else if repaired.len() < tc_arr.len() {
                                repair_indices.push((idx, repaired));
                            }
                        }
                    }
                }
            }
            // Apply repairs in reverse order so earlier indices stay valid.
            for (idx, repaired) in repair_indices.into_iter().rev() {
                if repaired.is_empty() {
                    messages.remove(idx);
                    eprintln!("[tool_calls repair] removed orphan assistant at index {idx}");
                } else {
                    if let Some(msg) = messages.get_mut(idx) {
                        if let Some(obj) = msg.as_object_mut() {
                            obj.insert("tool_calls".to_string(), Value::Array(repaired));
                            eprintln!("[tool_calls repair] filtered tool_calls at index {idx}");
                        }
                    }
                }
            }

            let request_body = json!({
                "model": self.config.model,
                "messages": messages,
                "temperature": self.config.temperature,
                "max_tokens": self.config.max_tokens,
                "tools": tool_specs_to_request_payload(&tool_specs),
                "tool_choice": "auto",
                "response_format": {"type": "json_object"},
            });
            let parts =
                provider.complete(request_body, sink.clone(), self.config.to_transport())?;
            let (message, response_id, finish_reason) =
                (parts.message, parts.response_id, parts.finish_reason);

            emit_agent_diagnostic(format!(
                "agent_turn_result kind={} turn={} response_id={} tool_calls={} content_chars={} reasoning_chars={} finish_reason={}",
                self.kind,
                turn + 1,
                response_id.as_deref().unwrap_or("-"),
                message.tool_calls.len(),
                message.content.as_deref().map(str::len).unwrap_or(0),
                message.reasoning_content.as_deref().map(str::len).unwrap_or(0),
                finish_reason.as_deref().unwrap_or("-"),
            ));

            if !message.tool_calls.is_empty() {
                let available_tools = tool_specs
                    .iter()
                    .map(|tool| tool.name.to_string())
                    .collect::<BTreeSet<_>>();
                // Separate valid and unknown tool calls.
                // Unknown tools are NOT added to the tool_calls chain
                // (avoids DeepSeek tool_call_id mismatch errors).
                // Instead they're injected as user-message errors.
                let (valid_calls, unknown_calls): (Vec<_>, Vec<_>) = message
                    .tool_calls
                    .iter()
                    .partition(|tc| available_tools.contains(&tc.function.name));
                // Push assistant message with only valid tool_calls (if any).
                if !valid_calls.is_empty() {
                    let filtered = cordis_plugin_sdk::llm::LlmMessage {
                        content: message.content.clone(),
                        reasoning_content: message.reasoning_content.clone(),
                        tool_calls: valid_calls.iter().map(|&tc| tc.clone()).collect(),
                    };
                    messages.push(llm_message_to_request(&filtered));
                }
                // Inject errors for unknown tools BEFORE any assistant message.
                for tool_call in &unknown_calls {
                    let tool_name = &tool_call.function.name;
                    self.unknown_tool_strikes += 1;
                    let tool_list = available_tools
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                    let err_msg = if self.unknown_tool_strikes >= UNKNOWN_TOOL_STRIKE_LIMIT {
                        format!(
                            "STOP — tool '{tool_name}' does not exist. Strike {}/{}. You are in a {} session. Your ONLY tools: {}",
                            self.unknown_tool_strikes, UNKNOWN_TOOL_STRIKE_LIMIT,
                            backend.tool_scope_label(), tool_list,
                        )
                    } else {
                        format!(
                            "Tool '{tool_name}' does not exist (strike {}/{}). Available tools: {}",
                            self.unknown_tool_strikes, UNKNOWN_TOOL_STRIKE_LIMIT, tool_list,
                        )
                    };
                    let _ = writeln!(
                        std::io::stdout(),
                        "⚙ {} (rejected — unknown tool)",
                        tool_name
                    );
                    let _ = std::io::stdout().flush();
                    messages.push(json!({"role": "user", "content": err_msg}));
                }
                // One blank line before the tool call block.
                if !valid_calls.is_empty() {
                    let _ = writeln!(std::io::stdout());
                }
                for tool_call in &valid_calls {
                    // Announce tool execution in real-time.
                    let tool_args_preview: String =
                        serde_json::from_str::<Value>(&tool_call.function.arguments)
                            .ok()
                            .and_then(|v| serde_json::to_string(&v).ok())
                            .unwrap_or_else(|| tool_call.function.arguments.clone());
                    let _ = writeln!(
                        std::io::stdout(),
                        "⚙ {} {}",
                        tool_call.function.name,
                        tool_args_preview
                    );
                    let _ = std::io::stdout().flush();
                    let (event, tool_output) = execute_agent_tool_call(
                        backend,
                        &available_tools,
                        &self.kind,
                        tool_call,
                        &mut self.unknown_tool_strikes,
                    );
                    let event_name = event.name.clone();
                    let terminal_reply = event
                        .ok
                        .then_some(())
                        .and(event.output.as_ref())
                        .and_then(|output| backend.terminal_tool_reply(&event.name, output));
                    self.transcript.push(AgentTranscriptEntry::Tool {
                        name: event.name.clone(),
                        arguments: event.arguments.clone(),
                        ok: event.ok,
                        output: event.output.clone(),
                        error: event.error.clone(),
                    });
                    let tool_ok = event.ok;
                    let tool_err = event.error.clone();
                    // Log tool result for debugging.
                    let preview: String = tool_output.chars().take(300).collect();
                    let preview = preview.replace('\n', " ");
                    if tool_ok {
                        let _ = writeln!(std::io::stdout(), "  -> ok {}", preview);
                    } else {
                        let _ = writeln!(std::io::stdout(), "  -> FAIL {}", preview);
                    }
                    let _ = std::io::stdout().flush();
                    tool_events.push(event);
                    // Reset unknown-tool strikes when a legitimate tool succeeds.
                    if tool_ok {
                        self.unknown_tool_strikes = 0;
                    }
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call.id,
                        "content": tool_output,
                    }));
                    // Inject a prominent warning when tools fail so the Agent
                    // cannot silently continue after an error.
                    if !tool_ok {
                        let err = tool_err.as_deref().unwrap_or("unknown error");
                        messages.push(json!({
                            "role": "user",
                            "content": format!(
                                "🔴 TOOL '{event_name}' FAILED: {err}\n\
                                 Do NOT proceed as if it succeeded. Fix the cause \
                                 and retry, or use a different approach."
                            ),
                        }));
                    }
                    if let Some(reply_content) = terminal_reply {
                        if message
                            .tool_calls
                            .last()
                            .is_some_and(|last| last.id != tool_call.id)
                        {
                            return Err(RuntimeError::LlmResponseInvalid {
                                message: format!(
                                    "terminal agent tool {} must be the last tool call in a {} turn",
                                    event_name, self.kind
                                ),
                            });
                        }
                        // Check for late-arriving messages before replying.
                        let new_msgs = drain_inject_queue();
                        if !new_msgs.is_empty() {
                            for m in new_msgs {
                                if m.trim().is_empty() {
                                    continue;
                                }
                                messages.push(json!({"role": "user", "content": &m}));
                                self.transcript
                                    .push(AgentTranscriptEntry::User { content: m });
                            }
                            continue;
                        }
                        self.remember_exchange(
                            trimmed,
                            &reply_content,
                            message.reasoning_content.as_deref(),
                        );
                        self.completed_turns += 1;
                        self.transcript.push(AgentTranscriptEntry::Assistant {
                            content: reply_content.clone(),
                            response_id: response_id.clone(),
                        });
                        return Ok(AgentReply {
                            response_id,
                            content: reply_content,
                            tool_events,
                        });
                    }
                }
                continue;
            }

            if let Some(content) = message
                .content
                .as_deref()
                .map(str::trim)
                .filter(|content| !content.is_empty())
            {
                // Before returning, check if new messages arrived during this turn.
                let new_msgs = drain_inject_queue();
                if !new_msgs.is_empty() {
                    for m in new_msgs {
                        if m.trim().is_empty() {
                            continue;
                        }
                        messages.push(json!({"role": "user", "content": &m}));
                        self.transcript
                            .push(AgentTranscriptEntry::User { content: m });
                    }
                    continue; // re-enter loop with new messages
                }
                self.remember_exchange(trimmed, content, message.reasoning_content.as_deref());
                self.completed_turns += 1;
                self.transcript.push(AgentTranscriptEntry::Assistant {
                    content: content.to_string(),
                    response_id: response_id.clone(),
                });
                return Ok(AgentReply {
                    response_id,
                    content: content.to_string(),
                    tool_events,
                });
            }

            if message
                .reasoning_content
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                messages.push(llm_message_to_request(&message));
                self.reasoning_only_strikes += 1;
                if self.reasoning_only_strikes >= 3 {
                    backend.host().agent_send_warning_to_test_groups(
                        "⚠ Agent produced 3 consecutive reasoning-only responses — may be stuck.",
                    );
                    self.reasoning_only_strikes = 0;
                }
                messages.push(json!({
                    "role": "user",
                    "content": "[system] You produced reasoning but no response. Please output your answer or call a tool.",
                }));
                continue;
            }
            self.reasoning_only_strikes = 0;

            return Err(RuntimeError::LlmResponseInvalid {
                message: "agent response had neither tool_calls nor final content".to_string(),
            });
        }

        // P1-31: on turn overflow, preserve what was accumulated across
        // the aborted respond() so a retry with the same session id has
        // context. The 1-slot skip accounts for the leading system prompt
        // that `messages` starts with; `self.history` doesn't include it.
        let system_prompt_offset = 1;
        if messages.len() > self.history.len() + system_prompt_offset {
            let recovered: Vec<_> = messages
                .iter()
                .skip(system_prompt_offset)
                .skip(self.history.len())
                .cloned()
                .collect();
            // Kernel introspection tool call/result pairs describe the
            // agent's own internal state; drop them before they reach
            // cross-turn history (and, via `to_snapshot`, on-disk recovery
            // snapshots) so the LLM doesn't imitate diagnostic call patterns.
            let recovered = filter_kernel_introspection_messages(&recovered);
            for msg in &recovered {
                let content_est = msg
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(estimate_tokens)
                    .unwrap_or(0);
                self.estimated_tokens = self.estimated_tokens.saturating_add(content_est);
            }
            self.history.extend(recovered);
        }
        Err(RuntimeError::LlmResponseInvalid {
            message: format!(
                "agent exceeded safety turn limit {} without producing a final response",
                AGENT_MAX_TOOL_TURNS
            ),
        })
    }

    /// 用调用方给定的 provider 跑一轮。
    ///
    /// `respond` 是过渡期的便利入口（内建传输）；C5 删掉内建实现后只剩本函数。
    /// `sink` 为 `Some` 时 provider 可逐段推增量，忽略它也必须正确工作。
    pub(crate) fn respond_with_provider<B: AgentBackend + ?Sized>(
        &mut self,
        backend: &mut B,
        user_input: &str,
        provider: &dyn LlmProvider,
        sink: Option<std::sync::Arc<dyn crate::llm_sink::TokenSink>>,
    ) -> Result<AgentReply, RuntimeError> {
        let transcript_len_before = self.transcript.len();
        let result = self.respond_inner(backend, user_input, provider, sink);
        self.repair_transcript_on_error(transcript_len_before, &result);
        result
    }

    pub fn respond_with_runtime_host<H: AgentToolHost + ?Sized>(
        &mut self,
        host: &H,
        session_id: &str,
        user_input: &str,
    ) -> Result<AgentReply, RuntimeError> {
        let soul_key = self.soul_key.clone();
        let mut backend = RuntimeShellAgentBackend {
            host,
            session_id,
            soul_key,
        };
        self.respond(&mut backend, user_input)
    }

    /// 同上，但由调用方提供 provider（走 LLM 插件时用）。
    ///
    /// backend 的构造留在本模块：`RuntimeShellAgentBackend` 是私有类型，
    /// 让 host 去拼装它会迫使它变 public 且暴露内部字段。
    pub(crate) fn respond_with_runtime_host_via<H: AgentToolHost + ?Sized>(
        &mut self,
        host: &H,
        session_id: &str,
        user_input: &str,
        provider: &dyn LlmProvider,
        sink: Option<std::sync::Arc<dyn crate::llm_sink::TokenSink>>,
    ) -> Result<AgentReply, RuntimeError> {
        let soul_key = self.soul_key.clone();
        let mut backend = RuntimeShellAgentBackend {
            host,
            session_id,
            soul_key,
        };
        self.respond_with_provider(&mut backend, user_input, provider, sink)
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    pub fn compact_history(&mut self) -> (usize, usize) {
        // Trigger at ~800K tokens (DeepSeek has 1M context).
        const COMPACT_AT_MSGS: usize = 4000;
        const KEEP_RECENT: usize = 2000;

        let before_len = self.history.len();
        if self.history.len() <= COMPACT_AT_MSGS {
            return (before_len, before_len);
        }

        let discard = self.history.len() - KEEP_RECENT;
        let split_at = discard.next_multiple_of(2);
        let split_at = split_at.min(self.history.len().saturating_sub(2));
        if split_at == 0 {
            return (before_len, before_len);
        }

        let old_messages: Vec<_> = self.history.drain(0..split_at).collect();
        let mut summary_lines: Vec<String> = Vec::new();
        for msg in &old_messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                continue;
            }
            // P1-33: `content.chars().take(500)` is char-based, but the
            // `content.len() > 500` guard was byte-based. For Chinese /
            // multi-byte text the `…` suffix would fire inconsistently
            // (byte length always > char length here). Compare chars on
            // both sides so behaviour is uniform.
            let char_count = content.chars().count();
            let short: String = content.chars().take(500).collect();
            let suffix = if char_count > 500 { "…" } else { "" };
            summary_lines.push(format!("[{role}]: {short}{suffix}"));
        }
        let summary = summary_lines.join("\n");
        self.history.insert(
            0,
            json!({
                "role": "system",
                "content": format!(
                    "[Compressed history — {} earlier messages summarized below]\n{}",
                    old_messages.len(),
                    summary
                ),
            }),
        );
        // P1-32: `estimated_tokens` was only ever incremented in
        // `remember_exchange`, so once it crossed the 800K guard it
        // would trip every subsequent request forever — but compact
        // then early-exited (history.len() <= COMPACT_AT_MSGS) and did
        // nothing, making the whole compact path a permanent no-op.
        // Rebuild the estimate from what actually remains in `history`.
        self.estimated_tokens = self
            .history
            .iter()
            .map(|msg| {
                let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                estimate_tokens(content)
            })
            .sum();
        eprintln!(
            "agent: compacted {} old messages into summary ({} chars), keeping {} recent",
            old_messages.len(),
            summary.len(),
            self.history.len() - 1
        );
        (before_len, self.history.len())
    }

    fn remember_exchange(
        &mut self,
        user_input: &str,
        assistant_output: &str,
        reasoning: Option<&str>,
    ) {
        let mut assistant_msg = json!({
            "role": "assistant",
            "content": assistant_output,
        });
        if let Some(r) = reasoning {
            if !r.trim().is_empty() {
                assistant_msg["reasoning_content"] = Value::String(r.to_string());
            }
        }
        self.history.push(json!({
            "role": "user",
            "content": user_input,
        }));
        self.history.push(assistant_msg);
        // Update token estimate.
        self.estimated_tokens += estimate_tokens(user_input)
            + estimate_tokens(assistant_output)
            + reasoning.map(estimate_tokens).unwrap_or(0);

        // Drop oldest user+assistant pairs if we exceed the limit.
        while self.history.len() > AGENT_HISTORY_MESSAGE_LIMIT {
            // Always remove in pairs (user + assistant).
            self.history.drain(0..2.min(self.history.len()));
        }
    }

    /// Inject a user→assistant exchange into the agent's history without
    /// triggering an LLM call. Used by `/` shortcuts so the agent stays
    /// aware of direct invocations.
    pub fn inject_exchange(&mut self, user_input: &str, assistant_output: &str) {
        self.remember_exchange(user_input, assistant_output, None);
        self.transcript.push(AgentTranscriptEntry::User {
            content: user_input.to_string(),
        });
        self.transcript.push(AgentTranscriptEntry::Assistant {
            content: assistant_output.to_string(),
            response_id: None,
        });
    }

    /// Create a serializable snapshot of the current session state.
    pub fn to_snapshot(&self) -> AgentSessionSnapshot {
        AgentSessionSnapshot {
            kind: self.kind.clone(),
            config: self.config.clone(),
            history: self.history.clone(),
            transcript: self.transcript.clone(),
            completed_turns: self.completed_turns,
            estimated_tokens: self.estimated_tokens,
            reasoning_only_strikes: self.reasoning_only_strikes,
            unknown_tool_strikes: self.unknown_tool_strikes,
            soul_key: self.soul_key.clone(),
        }
    }

    /// Reconstruct an AgentSession from a snapshot.
    ///
    /// 磁盘 schema 未变：快照本就不存 HTTP client（拆分前是从 config 重建，
    /// 现在传输在插件里、连重建都不需要了）。
    ///
    /// P2-33 + P0-25: `api_key` is `#[serde(skip_serializing)]`, so a
    /// recovered snapshot arrives with `config.api_key = None`. 保持 None——
    /// provider 插件在请求时按 `api_key_env` 从环境读，恢复出来的会话因此拿到
    /// 本次启动的密钥，而不是烧在磁盘文件里的旧值。
    pub fn from_snapshot(snapshot: AgentSessionSnapshot) -> Result<Self, RuntimeError> {
        Ok(Self {
            kind: snapshot.kind,
            config: snapshot.config,
            history: snapshot.history,
            transcript: snapshot.transcript,
            completed_turns: snapshot.completed_turns,
            estimated_tokens: snapshot.estimated_tokens,
            reasoning_only_strikes: snapshot.reasoning_only_strikes,
            unknown_tool_strikes: snapshot.unknown_tool_strikes,
            soul_key: snapshot.soul_key,
        })
    }
}

impl ShellAgentSession {
    pub fn new(config: LlmApiConfig) -> Result<Self, RuntimeError> {
        Ok(Self {
            inner: AgentSession::new(config, "runtime_shell")?,
        })
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }

    pub fn status(&self) -> ShellAgentStatus {
        self.inner.status()
    }

    pub fn transcript(&self) -> &[AgentTranscriptEntry] {
        self.inner.transcript()
    }

    pub fn tool_execution_summary(&self) -> AgentToolExecutionSummary {
        self.inner.tool_execution_summary()
    }

    pub fn respond<H: AgentToolHost + ?Sized>(
        &mut self,
        host: &H,
        session_id: &str,
        user_input: &str,
    ) -> Result<ShellAgentReply, RuntimeError> {
        self.inner
            .respond_with_runtime_host(host, session_id, user_input)
    }

    /// 测试入口：用给定 provider 跑一轮。
    ///
    /// 机制测试（工具分派、终止工具排序、历史）考的是循环本身，不是 wire——
    /// 拆分后传输在插件里，这些测试改用 `FakeLlmProvider` 直接喂补全，不再起
    /// mock HTTP server 与解析 SSE。
    #[cfg(test)]
    fn respond_via<H: AgentToolHost + ?Sized>(
        &mut self,
        host: &H,
        session_id: &str,
        user_input: &str,
        provider: &dyn LlmProvider,
    ) -> Result<ShellAgentReply, RuntimeError> {
        self.inner
            .respond_with_runtime_host_via(host, session_id, user_input, provider, None)
    }

    #[cfg(test)]
    fn remember_exchange(&mut self, user_input: &str, assistant_output: &str) {
        self.inner
            .remember_exchange(user_input, assistant_output, None);
        self.inner.completed_turns += 1;
    }
}

/// 把一条助手消息序列化回请求里的 message 对象。
///
/// 由 `ChatMessage::to_request_message` 原样迁来——它只做 JSON 拼装、不含
/// wire 解析，因此随契约类型留在 kernel（构造请求体是 kernel 的职责）。
fn llm_message_to_request(message: &cordis_plugin_sdk::llm::LlmMessage) -> Value {
    let mut out = Map::new();
    out.insert("role".to_string(), Value::String("assistant".to_string()));
    let content_value = message
        .content
        .as_ref()
        .map(|content| Value::String(content.clone()))
        .unwrap_or_else(|| {
            if message.reasoning_content.is_some() || !message.tool_calls.is_empty() {
                Value::String(String::new())
            } else {
                Value::Null
            }
        });
    out.insert("content".to_string(), content_value);
    if let Some(reasoning_content) = message.reasoning_content.as_ref() {
        if !reasoning_content.trim().is_empty() {
            out.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning_content.clone()),
            );
        }
    }
    if !message.tool_calls.is_empty() {
        out.insert(
            "tool_calls".to_string(),
            serde_json::to_value(&message.tool_calls).unwrap_or(Value::Array(Vec::new())),
        );
    }
    Value::Object(out)
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
struct EmptyArgs {}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct InvokePluginArgs {
    plugin_path: String,
    node_id: String,
    payload_json: Value,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct ExecuteTargetArgs {
    node_fqn: String,
    payload_json: Value,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct ListDirectoryArgs {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct SearchCodeArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct ReplaceInFileArgs {
    path: String,
    find: String,
    replace: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct DeleteFileArgs {
    path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct RenameFileArgs {
    path: String,
    new_path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct MoveFileArgs {
    path: String,
    new_path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct CopyFileArgs {
    path: String,
    new_path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct BuildPluginsArgs {
    plugin_name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RunPluginTestArgs {
    #[serde(default)]
    command: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestIterationArgs {
    plugin_path: String,
    instruction: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePluginArgs {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

/// O批: set_soul deliberately has NO soul_key parameter — the scope is
/// injected from the session, so the LLM can't target another user.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetSoulArgs {
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct ReloadRuntimeArgs {
    plugin_path: String,
}

fn execute_agent_tool_call<B: AgentBackend + ?Sized>(
    backend: &mut B,
    available_tools: &BTreeSet<String>,
    session_kind: &str,
    tool_call: &cordis_plugin_sdk::llm::LlmToolCall,
    unknown_tool_strikes: &mut usize,
) -> (AgentToolEvent, String) {
    let tool_name = tool_call.function.name.clone();
    if !available_tools.contains(&tool_name) {
        // P1-36: this branch is dead in practice — `respond()` filters
        // unknown tools into `unknown_calls` before ever calling into
        // `execute_agent_tool_call`, and the strike counter is
        // incremented there. Keep the branch as a defensive fallback but
        // do NOT double-increment the counter here; a future refactor
        // that skips the outer filter would otherwise trip the strike
        // limit twice as fast as intended.
        debug_assert!(
            false,
            "execute_agent_tool_call reached with unknown tool {tool_name}; caller should filter"
        );
        let tool_list = available_tools
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let error = json!({
            "ok": false,
            "error": format!(
                "tool {tool_name} is not available in the current {} scope (strike {}/{})",
                backend.tool_scope_label(),
                *unknown_tool_strikes,
                UNKNOWN_TOOL_STRIKE_LIMIT,
            ),
            "session_kind": session_kind,
            "available_tools": available_tools.iter().cloned().collect::<Vec<_>>(),
        });
        let event = AgentToolEvent {
            name: tool_name,
            arguments: json!({}),
            ok: false,
            output: None,
            error: error
                .get("error")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        };
        let mut err_text = error.to_string();
        if *unknown_tool_strikes >= UNKNOWN_TOOL_STRIKE_LIMIT {
            err_text.push_str(&format!(
                "\n\nSTOP — you have called {} unsupported tools. You are in a {} session. Your ONLY available tools are: {}. Do NOT call any other tool name. If you need a capability not listed, tell the user you cannot do it.",
                *unknown_tool_strikes,
                backend.tool_scope_label(),
                tool_list,
            ));
        }
        return (event, err_text);
    }

    let args_json = if tool_call.function.arguments.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str::<Value>(&tool_call.function.arguments) {
            Ok(value) => value,
            Err(err) => {
                let recovery_hint = if matches!(
                    tool_name.as_str(),
                    "replace_files_exact" | "replace_file_exact"
                ) {
                    " Retry with valid JSON. If the batch got too large or only one file needs follow-up, reread the affected file and retry with a smaller replace_files_exact call or replace_file_exact."
                } else {
                    ""
                };
                let error = json!({
                    "ok": false,
                    "error": format!(
                        "tool {tool_name} received invalid JSON arguments: {err}{recovery_hint}"
                    ),
                });
                return (
                    AgentToolEvent {
                        name: tool_name,
                        arguments: json!({}),
                        ok: false,
                        output: None,
                        error: error
                            .get("error")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                    },
                    error.to_string(),
                );
            }
        }
    };

    match backend.execute_tool(&tool_name, args_json.clone()) {
        Ok(output) => {
            let wrapped = json!({
                "ok": true,
                "result": output,
            });
            (
                AgentToolEvent {
                    name: tool_name,
                    arguments: args_json,
                    ok: true,
                    output: wrapped.get("result").cloned(),
                    error: None,
                },
                wrapped.to_string(),
            )
        }
        Err(err) => {
            let wrapped = json!({
                "ok": false,
                "error": err.to_string(),
            });
            (
                AgentToolEvent {
                    name: tool_name,
                    arguments: args_json,
                    ok: false,
                    output: None,
                    error: Some(err.to_string()),
                },
                wrapped.to_string(),
            )
        }
    }
}

struct RuntimeShellAgentBackend<'a, H: AgentToolHost + ?Sized> {
    host: &'a H,
    session_id: &'a str,
    /// O批: soul scope of the owning session ("" = none).
    soul_key: String,
}

impl<'a, H: AgentToolHost + ?Sized> AgentBackend for RuntimeShellAgentBackend<'a, H> {
    type Host = H;
    fn host(&self) -> &H {
        self.host
    }
    /// O批: three-part prompt — base, soul overlay, plugin hints. The
    /// overlay sits between them so a persona can adjust tone/behaviour
    /// while plugin usage contracts still land last (highest recency).
    fn system_prompt(&self) -> String {
        let mut prompt = shell_agent_system_prompt();
        if !self.soul_key.is_empty() {
            if let Some(persona) = self.host.agent_soul_overlay(&self.soul_key) {
                if !persona.trim().is_empty() {
                    prompt.push_str("\n\n--- Persona (per-user soul) ---\n\n");
                    prompt.push_str(persona.trim());
                    prompt.push('\n');
                }
            }
        }
        let hints = self.host.agent_plugin_hints();
        if !hints.is_empty() {
            prompt.push_str("\n\n--- Plugin-specific instructions ---\n\n");
            for hint in &hints {
                prompt.push_str(hint);
                prompt.push_str("\n\n");
            }
        }
        prompt
    }

    fn tool_specs(&self) -> Vec<AgentToolSpec> {
        shell_agent_tools()
    }

    fn execute_tool(&mut self, name: &str, arguments: Value) -> Result<Value, RuntimeError> {
        match name {
            AGENT_TOOL_GET_RUNTIME_STATUS => {
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_runtime_status()
            }
            AGENT_TOOL_LIST_PLUGINS => {
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_list_plugins()
            }
            AGENT_TOOL_LIST_NODES => {
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_list_nodes()
            }
            AGENT_TOOL_GET_KERNEL_STATUS => {
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_kernel_status()
            }
            AGENT_TOOL_GET_KERNEL_ISSUES => {
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_kernel_issues()
            }
            AGENT_TOOL_RELOAD_RUNTIME => {
                let args = parse_tool_value_arguments::<ReloadRuntimeArgs>(arguments, name)?;
                self.host.agent_reload_runtime(&args.plugin_path)
            }
            AGENT_TOOL_BUILD_PLUGINS => {
                let args = parse_tool_value_arguments::<BuildPluginsArgs>(arguments, name)?;
                self.host.agent_build_plugins(&args.plugin_name)
            }
            AGENT_TOOL_INVOKE_PLUGIN => {
                let args = parse_tool_value_arguments::<InvokePluginArgs>(arguments, name)?;
                self.host
                    .agent_invoke_plugin(&args.plugin_path, &args.node_id, args.payload_json)
            }
            AGENT_TOOL_EXECUTE_TARGET => {
                let args = parse_tool_value_arguments::<ExecuteTargetArgs>(arguments, name)?;
                self.host
                    .agent_execute_target(&args.node_fqn, args.payload_json)
            }
            AGENT_TOOL_READ_FILE => {
                let args = parse_tool_value_arguments::<ReadFileArgs>(arguments, name)?;
                self.host
                    .agent_read_file(&args.path, args.offset, args.limit)
            }
            AGENT_TOOL_LIST_DIRECTORY => {
                let args = parse_tool_value_arguments::<ListDirectoryArgs>(arguments, name)?;
                self.host
                    .agent_list_directory(args.path.as_deref().unwrap_or("."))
            }
            AGENT_TOOL_SEARCH_CODE => {
                let args = parse_tool_value_arguments::<SearchCodeArgs>(arguments, name)?;
                self.host
                    .agent_search_code(&args.pattern, args.path.as_deref())
            }
            AGENT_TOOL_WRITE_FILE => {
                let args = parse_tool_value_arguments::<WriteFileArgs>(arguments, name)?;
                self.host.agent_write_file(&args.path, &args.content)
            }
            AGENT_TOOL_REPLACE_IN_FILE => {
                let args = parse_tool_value_arguments::<ReplaceInFileArgs>(arguments, name)?;
                self.host
                    .agent_replace_in_file(&args.path, &args.find, &args.replace)
            }
            AGENT_TOOL_REVERT_CHANGES => {
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_revert_changes()
            }
            AGENT_TOOL_DELETE_FILE => {
                let args = parse_tool_value_arguments::<DeleteFileArgs>(arguments, name)?;
                self.host.agent_delete_file(&args.path)
            }
            AGENT_TOOL_RENAME_FILE => {
                let args = parse_tool_value_arguments::<RenameFileArgs>(arguments, name)?;
                self.host.agent_rename_file(&args.path, &args.new_path)
            }
            AGENT_TOOL_MOVE_FILE => {
                let args = parse_tool_value_arguments::<MoveFileArgs>(arguments, name)?;
                self.host.agent_move_file(&args.path, &args.new_path)
            }
            AGENT_TOOL_COPY_FILE => {
                let args = parse_tool_value_arguments::<CopyFileArgs>(arguments, name)?;
                self.host.agent_copy_file(&args.path, &args.new_path)
            }
            AGENT_TOOL_COMPACT_CONTEXT => {
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_compact_context(self.session_id)
            }
            AGENT_TOOL_RUN_PLUGIN_TEST => {
                let args = parse_tool_value_arguments::<RunPluginTestArgs>(arguments, name)?;
                self.host.agent_run_plugin_test(args.command.as_deref())
            }
            AGENT_TOOL_REQUEST_ITERATION => {
                let args = parse_tool_value_arguments::<RequestIterationArgs>(arguments, name)?;
                self.host
                    .agent_request_iteration(&args.plugin_path, &args.instruction)
            }
            AGENT_TOOL_CREATE_PLUGIN => {
                let args = parse_tool_value_arguments::<CreatePluginArgs>(arguments, name)?;
                self.host
                    .agent_create_plugin(&args.name, args.description.as_deref())
            }
            AGENT_TOOL_SET_SOUL => {
                let args = parse_tool_value_arguments::<SetSoulArgs>(arguments, name)?;
                self.host.agent_set_soul(
                    &self.soul_key,
                    args.persona.as_deref(),
                    args.profile.as_deref(),
                )
            }
            other => Err(RuntimeError::InvalidArgument {
                message: format!("runtime shell agent does not support tool {other}"),
            }),
        }
    }

    fn tool_scope_label(&self) -> String {
        "runtime_shell".to_string()
    }
}

fn tool_specs_to_request_payload(specs: &[AgentToolSpec]) -> Vec<Value> {
    specs
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                },
            })
        })
        .collect()
}

fn shell_agent_tools() -> Vec<AgentToolSpec> {
    vec![
        AgentToolSpec {
            name: AGENT_TOOL_GET_RUNTIME_STATUS,
            description: "Get the current runtime host status, snapshot ids, candidate status, and recent reload reports.",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_LIST_PLUGINS,
            description: "List currently registered plugins, their load status, parent relationship, and known node ids.",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_LIST_NODES,
            description: "List currently registered node FQNs so you can choose a valid execute target.",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_GET_KERNEL_STATUS,
            description: "Get kernel status including plugin issue counts and blocked iteration counts.",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_GET_KERNEL_ISSUES,
            description: "List observed kernel plugin issues that may require iteration or reload investigation.",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_RELOAD_RUNTIME,
            description: "Reload the runtime snapshot and return the full reload diagnostics report. Use '/' to reload all plugins, or '/<path>' to reload a specific subtree (e.g. '/qq', '/expr'). Task services are gracefully restarted.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin_path": { "type": "string", "description": "Plugin path: '/' for all, '/qq' for a specific plugin or subtree." }
                },
                "required": ["plugin_path"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_BUILD_PLUGINS,
            description: "Build a plugin crate. Specify plugin_name (e.g. 'qq') or 'all' to build everything. Returns stdout, stderr, and exit_code.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin_name": { "type": "string", "description": "Plugin crate name or 'all'" },
                },
                "required": ["plugin_name"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_INVOKE_PLUGIN,
            description: "Invoke a plugin node directly by plugin path and node id.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin_path": { "type": "string" },
                    "node_id": { "type": "string" },
                    "payload_json": {
                        "type": "object",
                        "description": "JSON object payload for the plugin invoke request."
                    }
                },
                "required": ["plugin_path", "node_id", "payload_json"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_EXECUTE_TARGET,
            description: "Execute a registered node target through the runtime execution engine.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "node_fqn": { "type": "string" },
                    "payload_json": {
                        "type": "object",
                        "description": "JSON object payload for the execute request."
                    }
                },
                "required": ["node_fqn", "payload_json"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_READ_FILE,
            description: "Read a file within the fixtures workspace. Returns line-numbered content. Use offset/limit for large files.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the fixtures root." },
                    "offset": { "type": "integer", "description": "Optional 0-based line offset." },
                    "limit": { "type": "integer", "description": "Optional max lines to return." }
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_LIST_DIRECTORY,
            description: "List files and directories under a path within the fixtures workspace.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the fixtures root. Defaults to root." }
                },
                "required": [],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_SEARCH_CODE,
            description: "Search for a text pattern across source files in the fixtures workspace. Returns up to 40 matches with file path, line number, and line text.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Text pattern to search for (simple substring match)." },
                    "path": { "type": "string", "description": "Optional subdirectory to limit the search scope." }
                },
                "required": ["pattern"],
                "additionalProperties": false,
            }),
        },
        // Code editing tools are removed from RuntimeShell.
        // Plugin code changes must go through PluginIteration.
        AgentToolSpec {
            name: AGENT_TOOL_COMPACT_CONTEXT,
            description: "Compress the conversation history to save context space. Summarizes older messages, keeping the most recent exchanges intact. Use when the conversation is getting long to stay within the context window.",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_RUN_PLUGIN_TEST,
            description: "Run cargo test in the plugins workspace. Defaults to `cargo test --quiet --manifest-path plugins/Cargo.toml`. Pass a custom command to run a specific test (e.g. `cargo test -p gacha`). Use after making code edits to verify correctness.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Optional: custom cargo test command (default runs all plugin tests)" },
                },
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_REQUEST_ITERATION,
            description: "Start a PluginIteration session to safely modify plugin source code. Creates a backup snapshot before changes; on failure, auto-rollbacks to the snapshot. plugin_path: the plugin to modify (e.g. \"/web\"), or \"/\" for root workspace (entire plugins/ directory). instruction: what to change.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "plugin_path": { "type": "string", "description": "Target plugin path, e.g. \"/web\" or \"/qq\". Use \"/\" for root workspace to create new plugins or edit multiple plugins at once." },
                    "instruction": { "type": "string", "description": "What change to make in this iteration." },
                },
                "required": ["plugin_path", "instruction"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_CREATE_PLUGIN,
            description: "Create a new top-level plugin directory under plugins/ with a skeleton Cargo.toml, src/lib.rs, and add it to the workspace members. Use this before request_iteration to scaffold a brand-new plugin. name: plugin name (alphanumeric + underscore only), description: optional short description.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Plugin name. Must contain only letters, digits, and underscores (e.g. \"my_plugin\")." },
                    "description": { "type": "string", "description": "Optional short description for the plugin (goes in lib.rs doc comment)." },
                },
                "required": ["name"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_SET_SOUL,
            description: "Update the persona (soul) and/or LLM profile for the CURRENT user's current conversation scope. Use when the user asks to change your personality, tone, role, or which model profile serves them. persona: full replacement persona text (omit to keep). profile: named LLM profile like \"default\" or \"fast\" (omit to keep; empty string clears back to default). Changes take effect in NEW sessions (after /reset). You can never edit another user's soul — the scope is bound to this session.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "persona": { "type": "string", "description": "New persona overlay text for the system prompt. Replaces the existing persona entirely." },
                    "profile": { "type": "string", "description": "Named LLM profile for this user (must exist in llm_api.yaml profiles). Empty string resets to default." },
                },
                "additionalProperties": false,
            }),
        },
    ]
}

fn shell_agent_system_prompt() -> String {
    let today = Local::now().format("%Y-%m-%d").to_string();
    let mut prompt = format!("Today's date is {today}.\n\n");
    prompt.push_str("\
You are the Cordis shell agent running inside the cordis-runtime serve REPL.\n\
You can read source files, list directories, search code, inspect runtime status, list plugins/nodes, invoke plugins, execute targets, run plugin tests, and reload the runtime.\n\
To modify plugin source code, use `request_iteration` to start a safe PluginIteration session (supports rollback).\n\
\n\
Plugins may provide additional instructions (chat mode protocols, etc.) — see the \"plugin-specific instructions\" section below if present.\n\
\n\
Before calling any tool that may block for more than a moment, tell the user what you are about to do. If the platform you're talking through has a send-message ability (check available plugins), use it to send a brief heads-up. Then report the outcome when done. Never go silent for long.\n\
\n\
SAFETY RULES — never do these without explicit user request:\n\
- NEVER create Python scripts, shell scripts, or non-Rust files in the plugins directory. Plugins are Rust dylib crates only.\n\
- NEVER put output files, logs, or computed results inside any plugin directory.\n\
- For temporary files, use the `tmp/` directory at the workspace root. The tmp/ directory is gitignored.\n\
- NEVER leave temporary files behind. Delete them after use.\n\
- NEVER read, list, or write outside the project workspace. Read the project freely;\n\
  write only under plugins/. Never touch hidden config dirs, credentials, or system paths.\n\
- NEVER run commands that access credentials, SSH keys, tokens, or system files.\n\
- NEVER remove a plugin from its parent's `children` list in Cargo.toml.\n\
  Removing a child plugin declaration breaks the runtime plugin graph.\n\
- NEVER delete `docs/` directories or files (overview.md, interfaces.json).\n\
  These are scaffold artifacts required for plugin validation.\n\
- NEVER delete source files or test files that you did not create yourself.\n\
- NEVER modify `Cargo.toml` files beyond adding new dependencies or children\n\
  you are explicitly told to create.\n\
- If a build fails, fix YOUR changes — don't remove pre-existing code to\n\
  make it compile.\n\
\n\
IMPORTANT — workspace layout:\n\
- The plugins workspace is under the `plugins/` directory.\n\
- ALWAYS run cargo commands from the plugins directory: `cd plugins && cargo ...`\n\
  Example: `cd plugins && cargo build 2>&1`\n\
  Example: `cd plugins && cargo test -p expr 2>&1`\n\
- Plugin source files are under `plugins/<name>/src/`, e.g. `plugins/expr/src/lib.rs`.\n\
- The fixtures root is `./`, but cargo needs `plugins/` as the working directory.\n\
- When creating NEW files/directories under plugins/, use write_file or invoke_plugin\n\
  (e.g. `mkdir -p plugins/expr/evaluator/pow/src`) first — write_file may reject non-existent paths.\n\
- After editing plugin code, use `build_plugins` with `plugin_name: \"<name>\"` (or `\"all\"`)\n\
  to compile and sync the .so to artifacts/ automatically.\n\
- Then use `reload_runtime` with `plugin_path: \"/<name>\"` (or `\"/\"` for all) to load\n\
  changes into the live runtime.\n\
- Use `run_plugin_test` to run cargo test (defaults to all plugins; pass a custom command\n\
  like `cargo test -p gacha` for a specific plugin).\n\
\n\
CREATING A NEW PLUGIN — required files (every new plugin MUST have all of these):\n\
  plugins/<name>/Cargo.toml      — package name = plugin_path = directory name (normalised)\n\
  plugins/<name>/src/lib.rs       — Rust source with export_plugin_api! macro\n\
  plugins/<name>/tests/           — at least one .rs test file (e.g. tests/basic.rs)\n\
  plugins/<name>/docs/human/overview.md  — one-paragraph summary of what the plugin does\n\
  plugins/<name>/docs/agent/interfaces.json — node docs.  build_plugins regenerates\n\
    this file from the plugin's docs_value().  After ANY source change that affects\n\
    node summaries, schemas, or side_effects, run build_plugins to refresh it.\n\
\n\
Cargo.toml MUST include these sections:\n\
  [package.metadata.cordis]\n\
    plugin_path = \"<name>\"       — same as directory name\n\
    abi_kind = \"rust\"\n\
    declared_nodes = [...]        — list every node_id your plugin exports\n\
    children = []                 — (unless the plugin has child plugins)\n\
  [package.metadata.cordis.abi_fingerprint]\n\
    crate_hash = \"crate_<name>_v1\"\n\
    api_hash = \"api_v2\"           — MUST be \"api_v2\" (same as all other plugins)\n\
    (do NOT declare rustc_version/target_triple — they default to the\n\
     current toolchain; in code use AbiFingerprint::current_build)\n\
\n\
Plugins workspace membership:\n\
  After creating a new plugin, add \"<name>\" to the `members` list in plugins/Cargo.toml.\n\
  After everything compiles, run `build_plugins` with `plugin_name: \"all\"` to sync artifacts.\n\
  Then `reload_runtime` with `plugin_path: \"/\"` to load the new plugin.\n\
\n\
When the user asks you to add a feature or fix a bug in a plugin:\n\
1. Read the relevant source files to understand the codebase structure.\n\
2. Call `request_iteration(plugin_path, instruction)` to start a PluginIteration session.\n\
   This snapshots the plugin, scopes edits to the subtree, and enables rollback on failure.\n\
3. Inside the iteration, edit, build, test, and reload using iteration tools.\n\
4. The iteration auto-commits on success or rolls back on failure.\n\
You cannot edit plugin code directly — always use request_iteration.\n\
Prefer concise, operator-friendly replies. Mention important tool outcomes plainly.\n\
Do not invent runtime state or claim a command succeeded unless a tool confirmed it.\n\
\n\
CRITICAL: Your final output must be {\"action\":\"suspend\"} (JSON, nothing else).\n\
To send a reply, use the invoke_plugin tool to call qq_send instead of outputting text.\n\
Never output plain text — it will be dropped. All communication goes through tools.\n\
\n\
CRITICAL — YOUR TOOLS (only these exist; all others will fail immediately):\n\
  get_runtime_status, list_plugins, list_nodes, get_kernel_status, get_kernel_issues,\n\
  reload_runtime, build_plugins, invoke_plugin, execute_target, read_file, search_code,\n\
  compact_context, list_directory, run_plugin_test, request_iteration\n\
\n\
For build: use build_plugins.  For testing: use run_plugin_test (defaults to all plugin tests;\n\
pass a custom command for specific tests, e.g. `cargo test -p gacha`).\n\
For plugin node calls: always use invoke_plugin(plugin_path, node_id, payload_json)."
    );
    prompt
}

fn emit_agent_diagnostic(message: String) {
    if env::var(LLM_DEBUG_ENV).ok().as_deref() == Some("1") {
        eprintln!("{message}");
    }
}

fn parse_json_or_string(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

fn parse_tool_value_arguments<T>(args: Value, tool_name: &str) -> Result<T, RuntimeError>
where
    T: DeserializeOwned,
{
    serde_json::from_value::<T>(args).map_err(|err| RuntimeError::LlmResponseInvalid {
        message: format!("shell agent tool {tool_name} had invalid arguments: {err}"),
    })
}

fn to_json_value<T: Serialize>(label: &str, value: T) -> Result<Value, RuntimeError> {
    serde_json::to_value(value).map_err(|err| RuntimeError::Invariant {
        message: format!("failed to serialize {label}: {err}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TOOL_RECORD: &str = "record_summary";
    const TEST_TOOL_READ: &str = "read_context";

    /// 按脚本回放补全的假 provider：让机制测试（工具分派、历史、计数）不再需要
    /// 起 TCP mock server 与解析 SSE。
    ///
    /// 每次 `complete` 弹出一条预置回复；`sink` 非空时按 `stream_as` 逐段推增量，
    /// 用于验证"展示由调用方决定"这条契约。
    struct FakeLlmProvider {
        replies: std::sync::Mutex<std::collections::VecDeque<cordis_plugin_sdk::llm::LlmMessage>>,
        /// 依次推给 sink 的 (是否推理, 文本) 增量。
        stream_as: Vec<(bool, String)>,
        /// 记录收到的请求体，供断言。
        seen_bodies: std::sync::Mutex<Vec<Value>>,
    }

    impl FakeLlmProvider {
        fn new(replies: Vec<cordis_plugin_sdk::llm::LlmMessage>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.into()),
                stream_as: Vec::new(),
                seen_bodies: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn streaming(
            replies: Vec<cordis_plugin_sdk::llm::LlmMessage>,
            stream_as: Vec<(bool, String)>,
        ) -> Self {
            Self {
                stream_as,
                ..Self::new(replies)
            }
        }
    }

    impl LlmProvider for FakeLlmProvider {
        fn complete(
            &self,
            body: Value,
            sink: Option<std::sync::Arc<dyn crate::llm_sink::TokenSink>>,
            _transport: cordis_plugin_sdk::llm::LlmTransportConfig,
        ) -> Result<LlmCompletionParts, RuntimeError> {
            self.seen_bodies.lock().expect("seen_bodies").push(body);
            if let Some(sink) = sink.as_ref() {
                for (is_reasoning, text) in &self.stream_as {
                    if *is_reasoning {
                        sink.on_reasoning(text);
                    } else {
                        sink.on_content(text);
                    }
                }
            }
            let message = self
                .replies
                .lock()
                .expect("replies")
                .pop_front()
                .ok_or_else(|| RuntimeError::LlmRequestFailed {
                    message: "fake provider ran out of scripted replies".to_string(),
                })?;
            let finish = if message.tool_calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            };
            Ok(LlmCompletionParts {
                message,
                response_id: Some("resp-fake".to_string()),
                finish_reason: Some(finish.into()),
            })
        }
    }

    /// 记录增量的 sink，用于断言顺序与分类。
    #[derive(Default)]
    struct RecordingTokenSink {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl crate::llm_sink::TokenSink for RecordingTokenSink {
        fn on_reasoning(&self, delta: &str) {
            self.events
                .lock()
                .expect("events")
                .push(format!("r:{delta}"));
        }
        fn on_content(&self, delta: &str) {
            self.events
                .lock()
                .expect("events")
                .push(format!("c:{delta}"));
        }
    }

    fn assistant_reply(content: &str) -> cordis_plugin_sdk::llm::LlmMessage {
        cordis_plugin_sdk::llm::LlmMessage {
            content: Some(content.to_string()),
            ..Default::default()
        }
    }

    /// 接缝契约：provider 拿到 kernel 构造的请求体，其返回值即权威结果；
    /// sink 只是旁路，忽略它也必须得到同样的回复。
    #[test]
    fn provider_seam_returns_reply_and_sink_is_optional() {
        let provider = FakeLlmProvider::new(vec![assistant_reply("hello")]);
        let parts = provider
            .complete(json!({"model": "m"}), None, Default::default())
            .expect("complete");
        let (message, response_id, finish) =
            (parts.message, parts.response_id, parts.finish_reason);
        assert_eq!(message.content.as_deref(), Some("hello"));
        assert_eq!(response_id.as_deref(), Some("resp-fake"));
        assert_eq!(finish.as_deref(), Some("stop"));
        // kernel 构造的 body 原样抵达 provider。
        let bodies = provider.seen_bodies.lock().expect("bodies");
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].get("model").and_then(|v| v.as_str()), Some("m"));
    }

    /// 有 sink 时增量按序抵达，且**完整内容仍在返回值里**——REPL 靠增量显示，
    /// inbox/serve 靠返回值，两条路径读的是同一次补全。
    #[test]
    fn provider_seam_streams_deltas_to_sink_in_order() {
        let provider = FakeLlmProvider::streaming(
            vec![assistant_reply("done")],
            vec![
                (true, "thinking".to_string()),
                (false, "par".to_string()),
                (false, "tial".to_string()),
            ],
        );
        let sink = std::sync::Arc::new(RecordingTokenSink::default());
        let parts = provider
            .complete(
                json!({}),
                Some(sink.clone() as std::sync::Arc<dyn crate::llm_sink::TokenSink>),
                Default::default(),
            )
            .expect("complete");
        assert_eq!(
            *sink.events.lock().expect("events"),
            vec!["r:thinking", "c:par", "c:tial"]
        );
        assert_eq!(parts.message.content.as_deref(), Some("done"));
    }

    /// provider 用尽脚本回复时报错，而不是静默返回空消息。
    #[test]
    fn provider_seam_surfaces_exhausted_script_as_error() {
        let provider = FakeLlmProvider::new(Vec::new());
        let err = provider
            .complete(json!({}), None, Default::default())
            .expect_err("must fail");
        assert!(
            matches!(err, RuntimeError::LlmRequestFailed { .. }),
            "{err:?}"
        );
    }

    #[derive(Default)]
    struct FakeHost;

    impl AgentToolHost for FakeHost {
        fn agent_runtime_status(&self) -> Result<Value, RuntimeError> {
            Ok(json!({
                "current_snapshot_id": "snapshot-demo",
                "plugin_count": 3,
            }))
        }

        fn agent_list_plugins(&self) -> Result<Value, RuntimeError> {
            Ok(json!({
                "plugins": [
                    { "plugin_path": "expr", "node_ids": ["expr_entry"] }
                ]
            }))
        }

        fn agent_list_nodes(&self) -> Result<Value, RuntimeError> {
            Ok(json!({
                "nodes": [
                    { "node_fqn": "expr::expr_entry", "plugin_path": "expr", "node_id": "expr_entry" }
                ]
            }))
        }

        fn agent_kernel_status(&self) -> Result<Value, RuntimeError> {
            Ok(json!({ "plugin_issue_count": 0 }))
        }

        fn agent_kernel_issues(&self) -> Result<Value, RuntimeError> {
            Ok(json!([]))
        }

        fn agent_reload_runtime(&self, _plugin_path: &str) -> Result<Value, RuntimeError> {
            Ok(json!({ "ok": true }))
        }

        fn agent_invoke_plugin(
            &self,
            plugin_path: &str,
            node_id: &str,
            payload_json: Value,
        ) -> Result<Value, RuntimeError> {
            Ok(json!({
                "plugin_path": plugin_path,
                "node_id": node_id,
                "payload": payload_json,
            }))
        }

        fn agent_execute_target(
            &self,
            node_fqn: &str,
            payload_json: Value,
        ) -> Result<Value, RuntimeError> {
            Ok(json!({
                "node_fqn": node_fqn,
                "payload": payload_json,
            }))
        }

        fn agent_read_file(
            &self,
            path: &str,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<Value, RuntimeError> {
            Ok(json!({
                "path": path,
                "total_lines": 1,
                "lines": [{"line": 1, "text": "fake content"}],
            }))
        }

        fn agent_list_directory(&self, path: &str) -> Result<Value, RuntimeError> {
            Ok(json!({
                "path": path,
                "entries": [{"name": "lib.rs", "kind": "file"}],
            }))
        }

        fn agent_search_code(
            &self,
            pattern: &str,
            _path: Option<&str>,
        ) -> Result<Value, RuntimeError> {
            Ok(json!({
                "pattern": pattern,
                "matches": [],
            }))
        }

        fn agent_write_file(&self, path: &str, _content: &str) -> Result<Value, RuntimeError> {
            Ok(json!({ "path": path, "written_bytes": 0 }))
        }

        fn agent_replace_in_file(
            &self,
            path: &str,
            _find: &str,
            _replace: &str,
        ) -> Result<Value, RuntimeError> {
            Ok(json!({ "path": path, "replaced": true }))
        }

        fn agent_run_command(&self, _command: &str) -> Result<Value, RuntimeError> {
            Ok(json!({ "stdout": "", "stderr": "", "exit_code": 0 }))
        }

        fn agent_revert_changes(&self) -> Result<Value, RuntimeError> {
            Ok(json!({ "reverted_files": 0 }))
        }

        fn agent_delete_file(&self, path: &str) -> Result<Value, RuntimeError> {
            let _ = path;
            Ok(json!({ "deleted": true }))
        }

        fn agent_rename_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError> {
            let _ = (path, new_path);
            Ok(json!({ "renamed": true }))
        }

        fn agent_move_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError> {
            let _ = (path, new_path);
            Ok(json!({ "moved": true }))
        }

        fn agent_copy_file(&self, path: &str, new_path: &str) -> Result<Value, RuntimeError> {
            let _ = (path, new_path);
            Ok(json!({ "copied": true }))
        }

        fn agent_append_file(&self, _path: &str, content: &str) -> Result<Value, RuntimeError> {
            Ok(json!({ "appended_bytes": content.len() }))
        }

        fn agent_compact_context(&self, _session_id: &str) -> Result<Value, RuntimeError> {
            Ok(json!({ "compacted": true }))
        }

        fn agent_run_plugin_test(&self, _command: Option<&str>) -> Result<Value, RuntimeError> {
            Ok(json!({ "success": true, "stdout": "", "stderr": "" }))
        }

        fn agent_request_iteration(
            &self,
            _plugin_path: &str,
            _instruction: &str,
        ) -> Result<Value, RuntimeError> {
            Ok(json!({ "ok": true, "summary": "mock iteration", "verdict": "SimulatedSuccess" }))
        }

        fn agent_create_plugin(
            &self,
            name: &str,
            _description: Option<&str>,
        ) -> Result<Value, RuntimeError> {
            Ok(json!({ "ok": true, "plugin_path": format!("/{name}") }))
        }

        fn agent_send_warning_to_test_groups(&self, _message: &str) {}

        fn agent_build_plugins(&self, _plugin_name: &str) -> Result<Value, RuntimeError> {
            Ok(json!({ "ok": true, "exit_code": 0 }))
        }

        fn agent_plugin_hints(&self) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn reasoning_only_request_message_uses_empty_content() {
        let message = cordis_plugin_sdk::llm::LlmMessage {
            content: None,
            reasoning_content: Some("Need another reasoning pass".to_string()),
            tool_calls: Vec::new(),
        };
        let request = llm_message_to_request(&message);
        assert_eq!(
            request.get("role").and_then(Value::as_str),
            Some("assistant")
        );
        assert_eq!(request.get("content").and_then(Value::as_str), Some(""));
        assert_eq!(
            request.get("reasoning_content").and_then(Value::as_str),
            Some("Need another reasoning pass")
        );
    }

    #[test]
    fn shell_agent_reset_clears_history() {
        let config = LlmApiConfig {
            provider: "deepseek".to_string(),
            base_url: "http://127.0.0.1:12345/v1".to_string(),
            api_key: Some("test-key".to_string()),
            model: "deepseek-reasoner".to_string(),
            ..LlmApiConfig::default()
        };
        let mut session = ShellAgentSession::new(config).expect("build session");
        session.remember_exchange("hi", "hello");
        assert_eq!(session.status().completed_turns, 1);
        session.reset();
        assert_eq!(session.status().completed_turns, 0);
        assert_eq!(session.status().stored_messages, 0);
    }

    struct TerminalTestBackend {
        executed_tools: Vec<String>,
    }

    impl AgentBackend for TerminalTestBackend {
        type Host = FakeHost;
        fn host(&self) -> &FakeHost {
            &FakeHost
        }
        fn system_prompt(&self) -> String {
            "test backend".to_string()
        }

        fn tool_specs(&self) -> Vec<AgentToolSpec> {
            vec![
                AgentToolSpec {
                    name: TEST_TOOL_RECORD,
                    description: "Record a terminal summary.",
                    parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
                },
                AgentToolSpec {
                    name: TEST_TOOL_READ,
                    description: "Read extra context.",
                    parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
                },
            ]
        }

        fn execute_tool(&mut self, name: &str, _arguments: Value) -> Result<Value, RuntimeError> {
            self.executed_tools.push(name.to_string());
            Ok(json!({ "tool": name }))
        }

        fn terminal_tool_reply(&self, name: &str, _output: &Value) -> Option<String> {
            (name == TEST_TOOL_RECORD).then_some("Terminal summary recorded.".to_string())
        }
    }

    /// 循环会分派工具、把结果并回历史，并在下一轮拿到最终答复。
    /// 拆分后不再需要 mock HTTP：`FakeLlmProvider` 直接按脚本给补全。
    #[test]
    fn shell_agent_uses_runtime_tool_and_keeps_history() {
        let provider = FakeLlmProvider::new(vec![
            cordis_plugin_sdk::llm::LlmMessage {
                tool_calls: vec![cordis_plugin_sdk::llm::LlmToolCall {
                    id: "call_status".to_string(),
                    call_type: "function".to_string(),
                    function: cordis_plugin_sdk::llm::LlmToolFunction {
                        name: AGENT_TOOL_GET_RUNTIME_STATUS.to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
                ..Default::default()
            },
            assistant_reply("Runtime is healthy and loaded."),
        ]);
        let mut session = ShellAgentSession::new(LlmApiConfig::default()).expect("build session");
        let reply = session
            .respond_via(
                &FakeHost,
                "test-session-1",
                "What is the runtime status right now?",
                &provider,
            )
            .expect("agent reply");

        assert_eq!(reply.content, "Runtime is healthy and loaded.");
        assert_eq!(reply.tool_events.len(), 1);
        assert_eq!(reply.tool_events[0].name, AGENT_TOOL_GET_RUNTIME_STATUS);
        assert_eq!(session.status().completed_turns, 1);

        // 第一轮请求带工具规格；第二轮把工具结果并回了历史。
        let bodies = provider.seen_bodies.lock().expect("bodies");
        assert_eq!(bodies.len(), 2);
        assert!(
            bodies[0].get("tools").is_some(),
            "first turn must offer tools"
        );
        assert!(
            bodies[1].to_string().contains("snapshot-demo"),
            "second turn must carry the tool result back"
        );
    }

    /// 终止工具（`terminal_tool_reply` 返回 Some）一出现就结束会话，
    /// 不再向模型多要一轮。
    #[test]
    fn terminal_tool_reply_ends_agent_session_without_extra_turn() {
        let provider = FakeLlmProvider::new(vec![cordis_plugin_sdk::llm::LlmMessage {
            tool_calls: vec![cordis_plugin_sdk::llm::LlmToolCall {
                id: "call_record_summary".to_string(),
                call_type: "function".to_string(),
                function: cordis_plugin_sdk::llm::LlmToolFunction {
                    name: TEST_TOOL_RECORD.to_string(),
                    arguments: "{}".to_string(),
                },
            }],
            ..Default::default()
        }]);
        let mut backend = TerminalTestBackend {
            executed_tools: Vec::new(),
        };
        let mut session =
            AgentSession::new(LlmApiConfig::default(), "test").expect("build session");
        let reply = session
            .respond_with_provider(&mut backend, "Finish the iteration", &provider, None)
            .expect("agent reply");

        assert_eq!(reply.content, "Terminal summary recorded.");
        assert_eq!(backend.executed_tools, vec![TEST_TOOL_RECORD.to_string()]);
        // 关键：只发了一次补全请求——终止工具没有触发第二轮。
        assert_eq!(provider.seen_bodies.lock().expect("bodies").len(), 1);
    }

    /// 终止工具必须是本轮最后一个 tool_call：排在它后面的调用不执行，
    /// 否则会话已结束却还在动工具。
    #[test]
    fn terminal_tool_must_be_last_tool_call_in_turn() {
        let provider = FakeLlmProvider::new(vec![cordis_plugin_sdk::llm::LlmMessage {
            tool_calls: vec![
                cordis_plugin_sdk::llm::LlmToolCall {
                    id: "call_record_summary".to_string(),
                    call_type: "function".to_string(),
                    function: cordis_plugin_sdk::llm::LlmToolFunction {
                        name: TEST_TOOL_RECORD.to_string(),
                        arguments: "{}".to_string(),
                    },
                },
                cordis_plugin_sdk::llm::LlmToolCall {
                    id: "call_read_context".to_string(),
                    call_type: "function".to_string(),
                    function: cordis_plugin_sdk::llm::LlmToolFunction {
                        name: TEST_TOOL_READ.to_string(),
                        arguments: "{}".to_string(),
                    },
                },
            ],
            ..Default::default()
        }]);
        let mut backend = TerminalTestBackend {
            executed_tools: Vec::new(),
        };
        let mut session =
            AgentSession::new(LlmApiConfig::default(), "test").expect("build session");
        let err = session
            .respond_with_provider(&mut backend, "Bad terminal ordering", &provider, None)
            .expect_err("a terminal tool followed by another call must be rejected");

        // 运行时拒绝整轮而不是"执行前半段"——半执行的轮次会让会话状态含糊。
        assert!(
            matches!(&err, RuntimeError::LlmResponseInvalid { message }
                if message.contains("must be the last tool call")),
            "unexpected error: {err:?}"
        );
    }

    fn test_config() -> LlmApiConfig {
        LlmApiConfig {
            provider: "deepseek".to_string(),
            base_url: "http://127.0.0.1:12345/v1".to_string(),
            api_key: Some("test-key".to_string()),
            model: "deepseek-reasoner".to_string(),
            ..LlmApiConfig::default()
        }
    }

    /// P1-32: compact_history must be a no-op below COMPACT_AT_MSGS
    /// and return (before_len, before_len).
    #[test]
    fn compact_history_is_noop_below_threshold() {
        let mut session = AgentSession::new(test_config(), "runtime_shell").unwrap();
        // Add far fewer than COMPACT_AT_MSGS (4000).
        for i in 0..10 {
            session.remember_exchange(&format!("u{i}"), &format!("a{i}"), None);
        }
        let before = session.status().stored_messages;
        let (old_len, new_len) = session.compact_history();
        assert_eq!(old_len, before);
        assert_eq!(new_len, before);
    }

    /// P1-32 + P1-33: after crossing threshold, compact returns
    /// (before > after) and resets `estimated_tokens` to sum of what
    /// remains. Also verifies char-based truncation for multi-byte
    /// text.
    #[test]
    fn compact_history_shrinks_and_resets_estimated_tokens() {
        let mut session = AgentSession::new(test_config(), "runtime_shell").unwrap();
        // 4000+ msgs to cross COMPACT_AT_MSGS; include some Chinese
        // text so the char/byte guard (P1-33) is exercised.
        for i in 0..2200 {
            session.remember_exchange(&format!("请求{i}"), &format!("回复{i}"), None);
        }
        let before = session.status().stored_messages;
        assert!(before > 4000, "sanity: {before} messages");
        let (old_len, new_len) = session.compact_history();
        assert_eq!(old_len, before);
        assert!(new_len < old_len);
        assert!(new_len > 0);
        // estimated_tokens should be non-zero but bounded by the
        // remaining history — no runaway accumulation.
        let est = session.to_snapshot().estimated_tokens;
        assert!(
            est < old_len * 200, // upper-bound sanity check
            "estimated_tokens={est}, expected < {}",
            old_len * 200
        );
    }

    #[test]
    fn estimate_tokens_scales_with_length() {
        assert_eq!(estimate_tokens(""), 0);
        let small = estimate_tokens("hello world");
        let large = estimate_tokens(&"hello world ".repeat(100));
        assert!(large > small * 50, "large={large} small={small}");
    }

    /// A turn consisting solely of a kernel introspection call and its
    /// result must leave no trace in persisted history.
    #[test]
    fn filter_drops_pure_introspection_turn() {
        let turn = vec![
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": AGENT_TOOL_GET_RUNTIME_STATUS, "arguments": "{}" }
                    }
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": "{\"plugin_count\":3}"
            }),
        ];
        let filtered = filter_kernel_introspection_messages(&turn);
        assert!(
            filtered.is_empty(),
            "pure introspection turn should be fully dropped, got {filtered:?}"
        );
    }

    /// A turn mixing an introspection call with a conversational tool call
    /// must drop only the introspection call/result: the assistant message
    /// survives with its tool_calls array shrunk to the conversational call,
    /// and the conversational tool result is preserved.
    #[test]
    fn filter_shrinks_mixed_turn() {
        let turn = vec![
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_kernel",
                        "type": "function",
                        "function": { "name": AGENT_TOOL_LIST_PLUGINS, "arguments": "{}" }
                    },
                    {
                        "id": "call_read",
                        "type": "function",
                        "function": { "name": AGENT_TOOL_READ_FILE, "arguments": "{\"path\":\"a\"}" }
                    }
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_kernel",
                "content": "{\"plugins\":[]}"
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_read",
                "content": "file body"
            }),
        ];
        let filtered = filter_kernel_introspection_messages(&turn);
        assert_eq!(
            filtered.len(),
            2,
            "expected assistant + read result: {filtered:?}"
        );
        let tool_calls = filtered[0]
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .expect("assistant message retains tool_calls");
        assert_eq!(tool_calls.len(), 1, "tool_calls array should shrink to 1");
        assert_eq!(
            tool_calls[0]
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str()),
            Some(AGENT_TOOL_READ_FILE),
        );
        assert_eq!(
            filtered[1].get("tool_call_id").and_then(|v| v.as_str()),
            Some("call_read"),
            "conversational tool result must be preserved"
        );
    }

    /// A turn with no kernel introspection tools must pass through the filter
    /// byte-for-byte unchanged.
    #[test]
    fn filter_preserves_ordinary_turn() {
        let turn = vec![
            json!({
                "role": "assistant",
                "content": "reading now",
                "tool_calls": [
                    {
                        "id": "call_read",
                        "type": "function",
                        "function": { "name": AGENT_TOOL_READ_FILE, "arguments": "{\"path\":\"a\"}" }
                    }
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_read",
                "content": "file body"
            }),
            json!({ "role": "user", "content": "follow-up" }),
        ];
        let filtered = filter_kernel_introspection_messages(&turn);
        assert_eq!(filtered, turn, "ordinary turn must be unchanged");
    }
}
