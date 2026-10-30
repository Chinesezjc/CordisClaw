//! Tool ABI and the host side of `AgentToolHost`.
//!
//! Kept in the kernel on purpose: tool names, argument shapes, dispatch and
//! `RuntimeHost`'s implementation are mechanism. The conversation loop that
//! calls them lives in `agent.rs` and moves to `cordis-agent`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use std::collections::BTreeSet;

use crate::agent::{SessionCaller, UNKNOWN_TOOL_STRIKE_LIMIT};
use crate::agent_api::{AgentBackend, AgentToolEvent, AgentToolHost};
use crate::core::error::RuntimeError;
use crate::host::RuntimeHost;

pub(crate) const AGENT_TOOL_GET_RUNTIME_STATUS: &str = "get_runtime_status";
pub(crate) const AGENT_TOOL_LIST_PLUGINS: &str = "list_plugins";
pub(crate) const AGENT_TOOL_LIST_NODES: &str = "list_nodes";
pub(crate) const AGENT_TOOL_GET_KERNEL_STATUS: &str = "get_kernel_status";
pub(crate) const AGENT_TOOL_GET_KERNEL_ISSUES: &str = "get_kernel_issues";
pub(crate) const AGENT_TOOL_RELOAD_RUNTIME: &str = "reload_runtime";
/// Kernel introspection tools — their output describes the agent's own
/// internal state (plugin counts, snapshot ids, kernel counters), not
/// conversational content. Their tool call/result pairs are stripped by
/// [`filter_kernel_introspection_messages`] before persisting to cross-turn
/// history so they don't teach the LLM to imitate diagnostic call patterns on
/// later turns.
pub(crate) const KERNEL_INTROSPECTION_TOOLS: &[&str] = &[
    AGENT_TOOL_GET_RUNTIME_STATUS,
    AGENT_TOOL_LIST_PLUGINS,
    AGENT_TOOL_LIST_NODES,
    AGENT_TOOL_GET_KERNEL_STATUS,
    AGENT_TOOL_GET_KERNEL_ISSUES,
];
/// Returns true if `name` is one of the kernel introspection tools.
pub(crate) fn is_kernel_introspection(name: &str) -> bool {
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
pub(crate) fn filter_kernel_introspection_messages(messages: &[Value]) -> Vec<Value> {
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
pub(crate) const AGENT_TOOL_BUILD_PLUGINS: &str = "build_plugins";
pub(crate) const AGENT_TOOL_INVOKE_PLUGIN: &str = "invoke_plugin";
pub(crate) const AGENT_TOOL_EXECUTE_TARGET: &str = "execute_target";
pub(crate) const AGENT_TOOL_READ_FILE: &str = "read_file";
pub(crate) const AGENT_TOOL_LIST_DIRECTORY: &str = "list_directory";
pub(crate) const AGENT_TOOL_SEARCH_CODE: &str = "search_code";
pub(crate) const AGENT_TOOL_WRITE_FILE: &str = "write_file";
pub(crate) const AGENT_TOOL_REPLACE_IN_FILE: &str = "replace_in_file";
pub(crate) const AGENT_TOOL_REVERT_CHANGES: &str = "revert_changes";
pub(crate) const AGENT_TOOL_DELETE_FILE: &str = "delete_file";
pub(crate) const AGENT_TOOL_RENAME_FILE: &str = "rename_file";
pub(crate) const AGENT_TOOL_MOVE_FILE: &str = "move_file";
pub(crate) const AGENT_TOOL_COPY_FILE: &str = "copy_file";
pub(crate) const AGENT_TOOL_COMPACT_CONTEXT: &str = "compact_context";
pub(crate) const AGENT_TOOL_RUN_PLUGIN_TEST: &str = "run_plugin_test";
pub(crate) const AGENT_TOOL_REQUEST_ITERATION: &str = "request_iteration";
pub(crate) const AGENT_TOOL_APPROVE_ITERATION: &str = "approve_iteration";
pub(crate) const AGENT_TOOL_CREATE_PLUGIN: &str = "create_plugin";
pub(crate) const AGENT_TOOL_SET_SOUL: &str = "set_soul";
pub(crate) const LLM_DEBUG_ENV: &str = "CORDIS_LLM_DEBUG";
impl AgentToolHost for RuntimeHost {
    /// 转发到 host 的真实实现。**少了这个转发，trait 的默认 no-op 会被调用**
    /// ——插件就再也收不到 `__meta`，鉴权会静默退化成"当作 CLI 操作员放行"。
    fn inject_turn_meta(&self, session_id: &str, payload: Value) -> Value {
        RuntimeHost::inject_turn_meta(self, session_id, payload)
    }

    /// P4: 转发到 host 的会话角色表。缺了它 `set_soul` 的群主门控会退回
    /// trait 默认的 `None`（= 判断不了就不限制），普通成员就能改本群人格。
    fn agent_session_caller(&self, session_id: &str) -> Option<SessionCaller> {
        let (is_group, is_admin) = self.session_caller_policy(session_id)?;
        Some(SessionCaller {
            is_group,
            is_admin,
            can_approve_iteration: self.session_approval_capability(session_id)?,
        })
    }

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
        // 目录直接由各插件的 node 文档渲染：id + 摘要 + 入参 + 出参。
        // 模型因此知道 `qq_get_group_info` 返回 `member_count` 之类的事实，
        // 插件不必在 system_hint 里再手写一遍（写过的会和实现漂移）。
        let mut catalog = String::from(crate::plugin::node_catalog::CATALOG_HEADER);
        for (path, plugin) in snapshot.plugin_registry().iter() {
            let Some(docs) = plugin.docs.as_ref() else {
                continue;
            };
            let nodes: Vec<&cordis_plugin_sdk::NodeDoc> = docs
                .nodes
                .iter()
                .filter(|node| node.agent_accessible)
                .collect();
            crate::plugin::node_catalog::push_plugin(&mut catalog, &path, &nodes);
        }
        let mut hints = vec![catalog];
        hints.extend(
            snapshot
                .plugin_registry()
                .iter()
                .filter_map(|(path, plugin)| {
                    let hint = plugin.docs.as_ref()?.system_hint.as_ref()?;
                    // Hints may carry `{setting:<key>}` placeholders that resolve
                    // against that plugin's `config/plugins/<name>.yaml` settings
                    // (e.g. qq's own uin), so a deployment value never has to be
                    // hard-coded in the plugin or resent every turn.
                    Some(snapshot.render_system_hint(&path, hint))
                }),
        );
        hints
    }

    fn agent_invoke_plugin(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload_json: Value,
    ) -> Result<Value, RuntimeError> {
        // A call recovered from serialised provider text names only the node
        // (the model never wrote a plugin path), so resolve it when exactly one
        // loaded plugin declares that node.
        let resolved = if plugin_path.is_empty() {
            RuntimeHost::plugin_for_node(self, node_id)?
        } else {
            plugin_path.to_string()
        };
        let plugin_path = resolved.as_str();
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
        let pp = plugin_path
            .trim()
            .trim_start_matches('/')
            .trim_end_matches('/');
        let pp = pp.strip_prefix("plugins/").unwrap_or(pp);
        // Empty plugin_path (from "/") means root workspace mode — the agent
        // can create/edit files anywhere under plugins/, not just one subtree.
        if !pp.is_empty() {
            let loaded: Vec<String> = self
                .current_snapshot()
                .plugin_registry()
                .iter()
                .map(|(path, _)| path)
                .collect();
            let known = loaded
                .iter()
                .any(|known| known == pp || known.starts_with(&format!("{pp}/")));
            if !known {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "unknown plugin path \"{plugin_path}\". Use \"/<plugin>\" for one plugin (e.g. \"/qq\") or \"/\" for the whole workspace. Loaded plugins: {}",
                        loaded.join(", ")
                    ),
                }));
            }
        }
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
            // Report the FINAL verdict, not just the verifier's. A rollout that
            // the canary rolled back (or that is blocked awaiting approval)
            // used to be reported as `verdict: Pass`, so the agent believed it
            // had changed files that were in fact restored.
            Ok(result) => Ok(crate::iteration::report::iteration_result_json(&result)),
            Err(e) => Ok(serde_json::json!({
                "ok": false,
                "error": e.to_string(),
            })),
        }
    }

    /// Approve a blocked iteration. The permission is a channel-provided fact
    /// (`SessionCaller::can_approve_iteration`): the kernel never decides who
    /// counts as an operator, it only refuses when the channel did not grant it.
    fn agent_approve_iteration(
        &self,
        session_id: &str,
        iteration_id: &str,
    ) -> Result<Value, RuntimeError> {
        let caller = self.agent_session_caller(session_id);
        crate::iteration::report::approve_iteration_permission(caller).map_err(|message| {
            RuntimeError::InvalidArgument {
                message: message.to_string(),
            }
        })?;
        let result = self.approve_blocked_iteration(iteration_id)?;
        Ok(serde_json::json!({
            "ok": true,
            "iteration_id": result.iteration_id,
            "verdict": format!("{:?}", result.final_verdict),
            "changed_paths": result.changed_paths,
            "summary": result.summary,
        }))
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
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub(crate) struct EmptyArgs {}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct InvokePluginArgs {
    pub(crate) plugin_path: String,
    pub(crate) node_id: String,
    pub(crate) payload_json: Value,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct ExecuteTargetArgs {
    pub(crate) node_fqn: String,
    pub(crate) payload_json: Value,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct ReadFileArgs {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) offset: Option<usize>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct ListDirectoryArgs {
    #[serde(default)]
    pub(crate) path: Option<String>,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct SearchCodeArgs {
    pub(crate) pattern: String,
    #[serde(default)]
    pub(crate) path: Option<String>,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct WriteFileArgs {
    pub(crate) path: String,
    pub(crate) content: String,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct ReplaceInFileArgs {
    pub(crate) path: String,
    pub(crate) find: String,
    pub(crate) replace: String,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct DeleteFileArgs {
    pub(crate) path: String,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct RenameFileArgs {
    pub(crate) path: String,
    pub(crate) new_path: String,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct MoveFileArgs {
    pub(crate) path: String,
    pub(crate) new_path: String,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct CopyFileArgs {
    pub(crate) path: String,
    pub(crate) new_path: String,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct BuildPluginsArgs {
    pub(crate) plugin_name: String,
}
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RunPluginTestArgs {
    #[serde(default)]
    pub(crate) command: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestIterationArgs {
    pub(crate) plugin_path: String,
    pub(crate) instruction: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApproveIterationArgs {
    pub(crate) iteration_id: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePluginArgs {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
}
/// O批: set_soul deliberately has NO soul_key parameter — the scope is
/// injected from the session, so the LLM can't target another user.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetSoulArgs {
    #[serde(default)]
    pub(crate) persona: Option<String>,
    #[serde(default)]
    pub(crate) profile: Option<String>,
}
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct ReloadRuntimeArgs {
    pub(crate) plugin_path: String,
}
pub(crate) fn execute_agent_tool_call<B: AgentBackend + ?Sized>(
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
pub(crate) fn parse_json_or_string(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}
pub(crate) fn parse_tool_value_arguments<T>(args: Value, tool_name: &str) -> Result<T, RuntimeError>
where
    T: DeserializeOwned,
{
    serde_json::from_value::<T>(args).map_err(|err| RuntimeError::LlmResponseInvalid {
        message: format!("shell agent tool {tool_name} had invalid arguments: {err}"),
    })
}
pub(crate) fn to_json_value<T: Serialize>(label: &str, value: T) -> Result<Value, RuntimeError> {
    serde_json::to_value(value).map_err(|err| RuntimeError::Invariant {
        message: format!("failed to serialize {label}: {err}"),
    })
}
