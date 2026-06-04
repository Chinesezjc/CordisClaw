use crate::config::LlmApiConfig;
use crate::core::error::RuntimeError;
use crate::host::RuntimeHost;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::thread;
use std::time::{Duration, Instant};

const AGENT_HISTORY_MESSAGE_LIMIT: usize = 24;
const AGENT_MAX_TOOL_TURNS: usize = 96;
const AGENT_REQUEST_MAX_ATTEMPTS: usize = 3;
const AGENT_REQUEST_RETRY_BACKOFF_MS: u64 = 500;
const AGENT_TOOL_GET_RUNTIME_STATUS: &str = "get_runtime_status";
const AGENT_TOOL_LIST_PLUGINS: &str = "list_plugins";
const AGENT_TOOL_LIST_NODES: &str = "list_nodes";
const AGENT_TOOL_GET_KERNEL_STATUS: &str = "get_kernel_status";
const AGENT_TOOL_GET_KERNEL_ISSUES: &str = "get_kernel_issues";
const AGENT_TOOL_RELOAD_RUNTIME: &str = "reload_runtime";
const AGENT_TOOL_INVOKE_PLUGIN: &str = "invoke_plugin";
const AGENT_TOOL_EXECUTE_TARGET: &str = "execute_target";
const AGENT_TOOL_READ_FILE: &str = "read_file";
const AGENT_TOOL_LIST_DIRECTORY: &str = "list_directory";
const AGENT_TOOL_SEARCH_CODE: &str = "search_code";
const AGENT_TOOL_WRITE_FILE: &str = "write_file";
const AGENT_TOOL_REPLACE_IN_FILE: &str = "replace_in_file";
const AGENT_TOOL_RUN_COMMAND: &str = "run_command";
const AGENT_TOOL_REVERT_CHANGES: &str = "revert_changes";
const LLM_DEBUG_ENV: &str = "CORDIS_LLM_DEBUG";

pub trait AgentToolHost {
    fn agent_runtime_status(&self) -> Result<Value, RuntimeError>;
    fn agent_list_plugins(&self) -> Result<Value, RuntimeError>;
    fn agent_list_nodes(&self) -> Result<Value, RuntimeError>;
    fn agent_kernel_status(&self) -> Result<Value, RuntimeError>;
    fn agent_kernel_issues(&self) -> Result<Value, RuntimeError>;
    fn agent_reload_runtime(&self) -> Result<Value, RuntimeError>;
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
    fn agent_search_code(
        &self,
        pattern: &str,
        path: Option<&str>,
    ) -> Result<Value, RuntimeError>;
    fn agent_write_file(&self, path: &str, content: &str) -> Result<Value, RuntimeError>;
    fn agent_replace_in_file(
        &self,
        path: &str,
        find: &str,
        replace: &str,
    ) -> Result<Value, RuntimeError>;
    fn agent_run_command(&self, command: &str) -> Result<Value, RuntimeError>;
    fn agent_revert_changes(&self) -> Result<Value, RuntimeError>;
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

    fn agent_reload_runtime(&self) -> Result<Value, RuntimeError> {
        to_json_value("reload diagnostics", self.reload_with_diagnostics())
    }

    fn agent_invoke_plugin(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload_json: Value,
    ) -> Result<Value, RuntimeError> {
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
        let response = self.execute(node_fqn, payload_json)?;
        to_json_value("execution result", response)
    }

    fn agent_read_file(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<Value, RuntimeError> {
        let resolved = self.resolve_sandboxed_path(path)?;
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
            .map(|(i, line)| {
                json!({"line": start + i + 1, "text": line})
            })
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

    fn agent_search_code(
        &self,
        pattern: &str,
        path: Option<&str>,
    ) -> Result<Value, RuntimeError> {
        let search_root = match path {
            Some(p) => self.resolve_sandboxed_path(p)?,
            None => self.fixtures_root().to_path_buf(),
        };
        let mut matches = Vec::new();
        let mut walked = 0usize;
        self.walk_code_files(&search_root, &mut |rel_path, abs_path| {
            walked += 1;
            let content = match std::fs::read_to_string(abs_path) {
                Ok(c) => c,
                Err(_) => return,
            };
            for (line_no, line_text) in content.lines().enumerate() {
                if line_text.contains(pattern) {
                    matches.push(json!({
                        "path": rel_path,
                        "line": line_no + 1,
                        "text": line_text.trim(),
                    }));
                    if matches.len() >= 40 {
                        break;
                    }
                }
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
        if !original.contains(find) {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "replace_in_file: pattern not found in {path}: {find}"
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
        }))
    }

    fn agent_run_command(&self, command: &str) -> Result<Value, RuntimeError> {
        use std::process::Command;
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
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
        *rollback = crate::kernel::plugin_iteration::PluginEditRollback::empty(self.fixtures_root());
        Ok(json!({
            "reverted_files": count,
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
    fn system_prompt(&self) -> String;
    fn tool_specs(&self) -> Vec<AgentToolSpec>;
    fn execute_tool(&mut self, name: &str, arguments: Value) -> Result<Value, RuntimeError>;
    fn terminal_tool_reply(&self, _name: &str, _output: &Value) -> Option<String> {
        None
    }
    fn tool_scope_label(&self) -> String {
        "agent".to_string()
    }
}

#[derive(Debug, Clone)]
pub struct AgentSession {
    kind: String,
    config: LlmApiConfig,
    client: Client,
    history: Vec<Value>,
    transcript: Vec<AgentTranscriptEntry>,
    completed_turns: usize,
}

pub type ShellAgentStatus = AgentSessionStatus;
pub type ShellAgentReply = AgentReply;

#[derive(Debug, Clone)]
pub struct ShellAgentSession {
    inner: AgentSession,
}

impl AgentSession {
    pub fn new(config: LlmApiConfig, kind: impl Into<String>) -> Result<Self, RuntimeError> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|err| RuntimeError::LlmRequestFailed {
                message: format!("failed to build agent HTTP client: {err}"),
            })?;
        Ok(Self {
            kind: kind.into(),
            config,
            client,
            history: Vec::new(),
            transcript: Vec::new(),
            completed_turns: 0,
        })
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.transcript.clear();
        self.completed_turns = 0;
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

    pub fn respond<B: AgentBackend + ?Sized>(
        &mut self,
        backend: &mut B,
        user_input: &str,
    ) -> Result<AgentReply, RuntimeError> {
        let trimmed = user_input.trim();
        if trimmed.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "agent input must not be empty".to_string(),
            });
        }

        let provider = self.config.provider.trim().to_ascii_lowercase();
        if provider != "deepseek" && provider != "openai" {
            return Err(RuntimeError::UnsupportedLlmProvider {
                provider: self.config.provider.clone(),
            });
        }

        let endpoint = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
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
            if turn_started.elapsed() >= Duration::from_millis(self.config.timeout_ms) {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!(
                        "agent exceeded total response budget after {} tool turns; elapsed_ms={} timeout_ms={}",
                        turn,
                        turn_started.elapsed().as_millis(),
                        self.config.timeout_ms,
                    ),
                });
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

            let request_body = json!({
                "model": self.config.model,
                "messages": messages,
                "temperature": self.config.temperature,
                "max_tokens": self.config.max_tokens,
                "tools": tool_specs_to_request_payload(&tool_specs),
                "tool_choice": "auto",
            });
            let (message, response_id, finish_reason) =
                self.send_chat_request(endpoint.clone(), request_body)?;

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
                messages.push(message.to_request_message());
                let available_tools = tool_specs
                    .iter()
                    .map(|tool| tool.name.to_string())
                    .collect::<BTreeSet<_>>();
                // One blank line before the tool call block.
                let _ = writeln!(std::io::stdout());
                for tool_call in &message.tool_calls {
                    // Announce tool execution in real-time.
                    let tool_args_preview: String = serde_json::from_str::<Value>(
                        &tool_call.function.arguments,
                    )
                    .ok()
                    .and_then(|v| {
                        serde_json::to_string(&v).ok()
                    })
                    .unwrap_or_else(|| tool_call.function.arguments.clone());
                    let _ = writeln!(
                        std::io::stdout(),
                        "⚙ {} {}",
                        tool_call.function.name,
                        tool_args_preview
                    );
                    let _ = std::io::stdout().flush();
                    let (event, tool_output) =
                        execute_agent_tool_call(backend, &available_tools, &self.kind, tool_call);
                    let event_name = event.name.clone();
                    let terminal_reply = event
                        .ok
                        .then_some(())
                        .and_then(|_| event.output.as_ref())
                        .and_then(|output| backend.terminal_tool_reply(&event.name, output));
                    self.transcript.push(AgentTranscriptEntry::Tool {
                        name: event.name.clone(),
                        arguments: event.arguments.clone(),
                        ok: event.ok,
                        output: event.output.clone(),
                        error: event.error.clone(),
                    });
                    tool_events.push(event);
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call.id,
                        "content": tool_output,
                    }));
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
                        self.remember_exchange(trimmed, &reply_content);
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
                self.remember_exchange(trimmed, content);
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

            if matches!(finish_reason.as_deref(), Some("length"))
                && message
                    .reasoning_content
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
            {
                messages.push(message.to_request_message());
                continue;
            }

            return Err(RuntimeError::LlmResponseInvalid {
                message: "agent response had neither tool_calls nor final content".to_string(),
            });
        }

        Err(RuntimeError::LlmResponseInvalid {
            message: format!(
                "agent exceeded safety turn limit {} without producing a final response",
                AGENT_MAX_TOOL_TURNS
            ),
        })
    }

    pub fn respond_with_runtime_host<H: AgentToolHost + ?Sized>(
        &mut self,
        host: &H,
        user_input: &str,
    ) -> Result<AgentReply, RuntimeError> {
        let mut backend = RuntimeShellAgentBackend { host };
        self.respond(&mut backend, user_input)
    }

    fn remember_exchange(&mut self, user_input: &str, assistant_output: &str) {
        self.history.push(json!({
            "role": "user",
            "content": user_input,
        }));
        self.history.push(json!({
            "role": "assistant",
            "content": assistant_output,
        }));
        while self.history.len() > AGENT_HISTORY_MESSAGE_LIMIT {
            let drain = self
                .history
                .len()
                .saturating_sub(AGENT_HISTORY_MESSAGE_LIMIT);
            let remove = if drain % 2 == 0 { drain } else { drain + 1 };
            self.history.drain(0..remove.min(self.history.len()));
        }
    }

    /// Inject a user→assistant exchange into the agent's history without
    /// triggering an LLM call. Used by `/` shortcuts so the agent stays
    /// aware of direct invocations.
    pub fn inject_exchange(&mut self, user_input: &str, assistant_output: &str) {
        self.remember_exchange(user_input, assistant_output);
        self.transcript.push(AgentTranscriptEntry::User {
            content: user_input.to_string(),
        });
        self.transcript.push(AgentTranscriptEntry::Assistant {
            content: assistant_output.to_string(),
            response_id: None,
        });
    }

    fn send_chat_request(
        &self,
        endpoint: String,
        mut request_body: Value,
    ) -> Result<(ChatMessage, Option<String>, Option<String>), RuntimeError> {
        request_body["stream"] = Value::Bool(true);
        let api_key = resolve_api_key(&self.config)?;
        let request_summary = summarize_request(&endpoint, &request_body, self.config.timeout_ms);
        let overall_started = Instant::now();

        emit_agent_diagnostic(format!(
            "agent_request_start attempts={} {}",
            AGENT_REQUEST_MAX_ATTEMPTS, request_summary
        ));

        for attempt in 1..=AGENT_REQUEST_MAX_ATTEMPTS {
            let attempt_started = Instant::now();
            let mut http_request = self
                .client
                .post(endpoint.clone())
                .header(CONTENT_TYPE, "application/json")
                .header(AUTHORIZATION, format!("Bearer {api_key}"));
            if let Some(org) = &self.config.organization {
                if !org.trim().is_empty() {
                    http_request = http_request.header("OpenAI-Organization", org);
                }
            }
            if let Some(project) = &self.config.project {
                if !project.trim().is_empty() {
                    http_request = http_request.header("OpenAI-Project", project);
                }
            }

            let response = match http_request.json(&request_body).send() {
                Ok(response) => response,
                Err(err) => {
                    let message = format!(
                        "shell agent request failed: attempt={attempt}/{AGENT_REQUEST_MAX_ATTEMPTS} phase=send elapsed_ms={} total_elapsed_ms={} {} detail={}",
                        attempt_started.elapsed().as_millis(),
                        overall_started.elapsed().as_millis(),
                        request_summary,
                        format_transport_error(&err, self.config.timeout_ms),
                    );
                    if attempt < AGENT_REQUEST_MAX_ATTEMPTS {
                        emit_agent_diagnostic(format!(
                            "{message} retry_backoff_ms={AGENT_REQUEST_RETRY_BACKOFF_MS}"
                        ));
                        thread::sleep(Duration::from_millis(AGENT_REQUEST_RETRY_BACKOFF_MS));
                        continue;
                    }
                    return Err(RuntimeError::LlmRequestFailed { message });
                }
            };

            let status = response.status();
            if !status.is_success() {
                let raw_body = match response.text() {
                    Ok(body) => body,
                    Err(err) => {
                        let message = format!(
                            "shell agent request failed: attempt={attempt}/{AGENT_REQUEST_MAX_ATTEMPTS} phase=read_error_body elapsed_ms={} total_elapsed_ms={} {} detail={}",
                            attempt_started.elapsed().as_millis(),
                            overall_started.elapsed().as_millis(),
                            request_summary,
                            format_transport_error(&err, self.config.timeout_ms),
                        );
                        if attempt < AGENT_REQUEST_MAX_ATTEMPTS {
                            emit_agent_diagnostic(format!(
                                "{message} retry_backoff_ms={AGENT_REQUEST_RETRY_BACKOFF_MS}"
                            ));
                            thread::sleep(Duration::from_millis(AGENT_REQUEST_RETRY_BACKOFF_MS));
                            continue;
                        }
                        return Err(RuntimeError::LlmRequestFailed { message });
                    }
                };

                let message = format!(
                    "shell agent request failed: attempt={attempt}/{AGENT_REQUEST_MAX_ATTEMPTS} phase=http_status status={} elapsed_ms={} total_elapsed_ms={} {} error={} body_preview={}",
                    status.as_u16(),
                    attempt_started.elapsed().as_millis(),
                    overall_started.elapsed().as_millis(),
                    request_summary,
                    extract_error_message(&raw_body)
                        .unwrap_or_else(|| format!("status={} body={}", status, raw_body.trim())),
                    truncate_for_error(&raw_body, 400),
                );
                if attempt < AGENT_REQUEST_MAX_ATTEMPTS
                    && (status.is_server_error() || status.as_u16() == 429)
                {
                    emit_agent_diagnostic(format!(
                        "{message} retry_backoff_ms={AGENT_REQUEST_RETRY_BACKOFF_MS}"
                    ));
                    thread::sleep(Duration::from_millis(AGENT_REQUEST_RETRY_BACKOFF_MS));
                    continue;
                }
                return Err(RuntimeError::LlmRequestFailed { message });
            }

            let streamed = match read_chat_stream(response, &request_summary, attempt) {
                Ok(streamed) => streamed,
                Err(ChatStreamReadError::Io(err)) => {
                    let message = format!(
                        "shell agent request failed: attempt={attempt}/{AGENT_REQUEST_MAX_ATTEMPTS} phase=read_stream elapsed_ms={} total_elapsed_ms={} {} detail={}",
                        attempt_started.elapsed().as_millis(),
                        overall_started.elapsed().as_millis(),
                        request_summary,
                        format_stream_error(&err, self.config.timeout_ms),
                    );
                    if attempt < AGENT_REQUEST_MAX_ATTEMPTS {
                        emit_agent_diagnostic(format!(
                            "{message} retry_backoff_ms={AGENT_REQUEST_RETRY_BACKOFF_MS}"
                        ));
                        thread::sleep(Duration::from_millis(AGENT_REQUEST_RETRY_BACKOFF_MS));
                        continue;
                    }
                    return Err(RuntimeError::LlmRequestFailed { message });
                }
                Err(ChatStreamReadError::InvalidResponse(message)) => {
                    return Err(RuntimeError::LlmResponseInvalid { message });
                }
            };

            emit_agent_diagnostic(format!(
                "agent_request_success attempt={attempt}/{AGENT_REQUEST_MAX_ATTEMPTS} status={} elapsed_ms={} total_elapsed_ms={} response_bytes={} stream_events={} stream_done={} {}",
                status.as_u16(),
                attempt_started.elapsed().as_millis(),
                overall_started.elapsed().as_millis(),
                streamed.raw_bytes,
                streamed.event_count,
                streamed.saw_done,
                request_summary,
            ));

            return Ok((
                streamed.message,
                streamed.response_id,
                streamed.finish_reason,
            ));
        }

        Err(RuntimeError::LlmRequestFailed {
            message: format!(
                "shell agent request exhausted retries without returning a streamed response: {}",
                request_summary
            ),
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
        user_input: &str,
    ) -> Result<ShellAgentReply, RuntimeError> {
        self.inner.respond_with_runtime_host(host, user_input)
    }

    #[cfg(test)]
    fn remember_exchange(&mut self, user_input: &str, assistant_output: &str) {
        self.inner.remember_exchange(user_input, assistant_output);
        self.inner.completed_turns += 1;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ToolFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ToolFunctionCall,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    fn to_request_message(&self) -> Value {
        let mut message = Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        let content_value = self
            .content
            .as_ref()
            .map(|content| Value::String(content.clone()))
            .unwrap_or_else(|| {
                if self.reasoning_content.is_some() || !self.tool_calls.is_empty() {
                    Value::String(String::new())
                } else {
                    Value::Null
                }
            });
        message.insert("content".to_string(), content_value);
        if let Some(reasoning_content) = self.reasoning_content.as_ref() {
            if !reasoning_content.trim().is_empty() {
                message.insert(
                    "reasoning_content".to_string(),
                    Value::String(reasoning_content.clone()),
                );
            }
        }
        if !self.tool_calls.is_empty() {
            message.insert(
                "tool_calls".to_string(),
                serde_json::to_value(&self.tool_calls).unwrap_or(Value::Array(Vec::new())),
            );
        }
        Value::Object(message)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
struct ChatChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<ChatChunkChoice>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
struct ChatChunkChoice {
    #[serde(default)]
    delta: ChatChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
struct ChatChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<ToolFunctionCallDelta>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
struct ToolFunctionCallDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct ChatMessageAccumulator {
    response_id: Option<String>,
    content: String,
    reasoning_content: String,
    tool_calls: Vec<ToolCallAccumulator>,
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    id: String,
    call_type: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct StreamEventSummary {
    delta_reasoning_chars: usize,
    delta_content_chars: usize,
    delta_tool_call_count: usize,
    finish_reason: Option<String>,
}

#[derive(Debug)]
struct ChatStreamReadResult {
    response_id: Option<String>,
    message: ChatMessage,
    raw_bytes: usize,
    event_count: usize,
    saw_done: bool,
    finish_reason: Option<String>,
}

#[derive(Debug)]
enum ChatStreamReadError {
    Io(std::io::Error),
    InvalidResponse(String),
}

impl ChatMessageAccumulator {
    fn apply_chunk(&mut self, chunk: ChatChunk) -> Result<StreamEventSummary, RuntimeError> {
        if self.response_id.is_none() {
            self.response_id = chunk.id;
        }

        let mut summary = StreamEventSummary::default();
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content {
                summary.delta_content_chars += content.chars().count();
                self.content.push_str(&content);
            }
            if let Some(reasoning_content) = choice.delta.reasoning_content {
                summary.delta_reasoning_chars += reasoning_content.chars().count();
                self.reasoning_content.push_str(&reasoning_content);
            }
            for tool_call in choice.delta.tool_calls {
                summary.delta_tool_call_count += 1;
                while self.tool_calls.len() <= tool_call.index {
                    self.tool_calls.push(ToolCallAccumulator::default());
                }
                let slot = &mut self.tool_calls[tool_call.index];
                if let Some(id) = tool_call.id {
                    merge_stream_field(&mut slot.id, &id, false);
                }
                if let Some(call_type) = tool_call.call_type {
                    merge_stream_field(&mut slot.call_type, &call_type, false);
                }
                if let Some(function) = tool_call.function {
                    if let Some(name) = function.name {
                        merge_stream_field(&mut slot.name, &name, false);
                    }
                    if let Some(arguments) = function.arguments {
                        merge_stream_field(&mut slot.arguments, &arguments, true);
                    }
                }
            }
            if choice.finish_reason.is_some() {
                summary.finish_reason = choice.finish_reason;
            }
        }

        Ok(summary)
    }

    fn finish(self) -> Result<(ChatMessage, Option<String>), RuntimeError> {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|tool| {
                !(tool.id.is_empty()
                    && tool.call_type.is_empty()
                    && tool.name.is_empty()
                    && tool.arguments.is_empty())
            })
            .map(|tool| {
                if tool.id.is_empty()
                    || tool.call_type.is_empty()
                    || tool.name.is_empty()
                    || tool.arguments.is_empty()
                {
                    return Err(RuntimeError::LlmResponseInvalid {
                        message: format!(
                            "streamed shell agent tool call was incomplete: id_present={} type_present={} name_present={} arguments_present={}",
                            !tool.id.is_empty(),
                            !tool.call_type.is_empty(),
                            !tool.name.is_empty(),
                            !tool.arguments.is_empty(),
                        ),
                    });
                }
                Ok(ToolCall {
                    id: tool.id,
                    call_type: tool.call_type,
                    function: ToolFunctionCall {
                        name: tool.name,
                        arguments: tool.arguments,
                    },
                })
            })
            .collect::<Result<Vec<_>, RuntimeError>>()?;

        Ok((
            ChatMessage {
                content: normalize_streamed_optional_text(self.content),
                reasoning_content: normalize_streamed_optional_text(self.reasoning_content),
                tool_calls,
            },
            self.response_id,
        ))
    }
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
struct RunCommandArgs {
    command: String,
}

fn execute_agent_tool_call<B: AgentBackend + ?Sized>(
    backend: &mut B,
    available_tools: &BTreeSet<String>,
    session_kind: &str,
    tool_call: &ToolCall,
) -> (AgentToolEvent, String) {
    let tool_name = tool_call.function.name.clone();
    if !available_tools.contains(&tool_name) {
        let error = json!({
            "ok": false,
            "error": format!(
                "tool {tool_name} is not available in the current {} scope",
                backend.tool_scope_label()
            ),
            "session_kind": session_kind,
            "available_tools": available_tools.iter().cloned().collect::<Vec<_>>(),
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

fn read_chat_stream(
    response: Response,
    request_summary: &str,
    attempt: usize,
) -> Result<ChatStreamReadResult, ChatStreamReadError> {
    let mut reader = BufReader::new(response);
    let mut raw_bytes = 0usize;
    let mut event_count = 0usize;
    let mut saw_done = false;
    let mut finish_reason = None;
    let mut pending_data_lines = Vec::new();
    let mut accumulator = ChatMessageAccumulator::default();
    // Track how much content has been flushed to stdout so we only print new text.
    let mut flushed_content_len = 0usize;
    let mut flushed_reasoning_len = 0usize;

    loop {
        let mut line = String::new();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(ChatStreamReadError::Io)?;
        raw_bytes += bytes_read;
        if bytes_read == 0 {
            if !pending_data_lines.is_empty() {
                saw_done = process_stream_event(
                    &pending_data_lines.join("\n"),
                    &mut accumulator,
                    &mut event_count,
                    &mut finish_reason,
                    request_summary,
                    attempt,
                )?;
            }
            break;
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if pending_data_lines.is_empty() {
                continue;
            }
            let event_payload = pending_data_lines.join("\n");
            pending_data_lines.clear();
            if process_stream_event(
                &event_payload,
                &mut accumulator,
                &mut event_count,
                &mut finish_reason,
                request_summary,
                attempt,
            )? {
                saw_done = true;
                break;
            }
            // Stream new content to the user in real-time.
            let new_reasoning = &accumulator.reasoning_content[flushed_reasoning_len..];
            if !new_reasoning.is_empty() {
                // First reasoning delta: print the prefix.
                if flushed_reasoning_len == 0 {
                    let _ = write!(std::io::stdout(), "\x1b[2m💭 ");
                }
                // Print reasoning continuously without newlines.
                let _ = write!(std::io::stdout(), "{new_reasoning}");
                let _ = std::io::stdout().flush();
                flushed_reasoning_len = accumulator.reasoning_content.len();
            }
            let new_content = &accumulator.content[flushed_content_len..];
            if !new_content.is_empty() {
                // First content after reasoning: close the dim span and add a blank line.
                if flushed_reasoning_len > 0 && flushed_content_len == 0 {
                    let _ = writeln!(std::io::stdout(), "\x1b[0m");
                }
                print!("{new_content}");
                let _ = std::io::stdout().flush();
                flushed_content_len = accumulator.content.len();
            }
            continue;
        }

        if let Some(data) = trimmed.strip_prefix("data:") {
            pending_data_lines.push(data.trim_start().to_string());
        }
    }

    // Final flush: print any remaining unprinted content.
    let new_reasoning = &accumulator.reasoning_content[flushed_reasoning_len..];
    if !new_reasoning.is_empty() {
        if flushed_reasoning_len == 0 {
            let _ = write!(std::io::stdout(), "\x1b[2m💭 ");
        }
        let _ = write!(std::io::stdout(), "{new_reasoning}");
        let _ = std::io::stdout().flush();
    }
    let new_content = &accumulator.content[flushed_content_len..];
    if !new_content.is_empty() {
        if flushed_reasoning_len > 0 && flushed_content_len == 0 {
            let _ = writeln!(std::io::stdout(), "\x1b[0m");
        }
        print!("{new_content}");
        let _ = std::io::stdout().flush();
    } else if flushed_reasoning_len > 0 {
        // Reasoning-only response (no content): close the dim span.
        let _ = writeln!(std::io::stdout(), "\x1b[0m");
    }

    let (message, response_id) = accumulator
        .finish()
        .map_err(|err| ChatStreamReadError::InvalidResponse(err.to_string()))?;
    Ok(ChatStreamReadResult {
        response_id,
        message,
        raw_bytes,
        event_count,
        saw_done,
        finish_reason,
    })
}

fn process_stream_event(
    payload: &str,
    accumulator: &mut ChatMessageAccumulator,
    event_count: &mut usize,
    finish_reason: &mut Option<String>,
    request_summary: &str,
    attempt: usize,
) -> Result<bool, ChatStreamReadError> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }
    if trimmed == "[DONE]" {
        emit_agent_diagnostic(format!(
            "agent_stream_done attempt={} events={} {}",
            attempt, *event_count, request_summary
        ));
        return Ok(true);
    }

    let chunk: ChatChunk = serde_json::from_str(trimmed).map_err(|err| {
        ChatStreamReadError::InvalidResponse(format!(
            "invalid streamed shell agent chunk JSON: {err}; body_preview={}",
            truncate_for_error(trimmed, 800)
        ))
    })?;
    let summary = accumulator
        .apply_chunk(chunk)
        .map_err(|err| ChatStreamReadError::InvalidResponse(err.to_string()))?;
    if let Some(reason) = summary.finish_reason.clone() {
        *finish_reason = Some(reason);
    }
    *event_count += 1;
    emit_agent_diagnostic(format!(
        "agent_stream_event attempt={} event={} delta_reasoning_chars={} delta_content_chars={} delta_tool_calls={} finish_reason={} total_reasoning_chars={} total_content_chars={} {}",
        attempt,
        *event_count,
        summary.delta_reasoning_chars,
        summary.delta_content_chars,
        summary.delta_tool_call_count,
        summary.finish_reason.as_deref().unwrap_or("-"),
        accumulator.reasoning_content.chars().count(),
        accumulator.content.chars().count(),
        request_summary,
    ));
    Ok(false)
}

struct RuntimeShellAgentBackend<'a, H: AgentToolHost + ?Sized> {
    host: &'a H,
}

impl<'a, H: AgentToolHost + ?Sized> AgentBackend for RuntimeShellAgentBackend<'a, H> {
    fn system_prompt(&self) -> String {
        shell_agent_system_prompt().to_string()
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
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_reload_runtime()
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
            AGENT_TOOL_RUN_COMMAND => {
                let args = parse_tool_value_arguments::<RunCommandArgs>(arguments, name)?;
                self.host.agent_run_command(&args.command)
            }
            AGENT_TOOL_REVERT_CHANGES => {
                parse_tool_value_arguments::<EmptyArgs>(arguments, name)?;
                self.host.agent_revert_changes()
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
            description: "Reload the runtime snapshot and return the full reload diagnostics report.",
            parameters: json!({
                "type": "object",
                "properties": {},
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
        AgentToolSpec {
            name: AGENT_TOOL_WRITE_FILE,
            description: "Create or overwrite a file in the fixtures workspace. The previous content is backed up and can be restored with revert_changes.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the fixtures root." },
                    "content": { "type": "string", "description": "Full file content to write." }
                },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_REPLACE_IN_FILE,
            description: "Find and replace the first occurrence of a string in a file. The previous content is backed up and can be restored with revert_changes.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the fixtures root." },
                    "find": { "type": "string", "description": "Exact string to find (first occurrence only)." },
                    "replace": { "type": "string", "description": "Replacement string." }
                },
                "required": ["path", "find", "replace"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_RUN_COMMAND,
            description: "Run a shell command inside the fixtures root directory. Use for cargo build, cargo test, cargo check, etc. Returns stdout, stderr, and exit code. Commands are NOT sandboxed beyond running in the fixtures directory.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute." }
                },
                "required": ["command"],
                "additionalProperties": false,
            }),
        },
        AgentToolSpec {
            name: AGENT_TOOL_REVERT_CHANGES,
            description: "Revert all file changes made by write_file and replace_in_file in this session. Restores all touched files to their original content.",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
    ]
}

fn shell_agent_system_prompt() -> &'static str {
    "You are the Cordis shell agent running inside the cordis-runtime serve REPL.\n\
You are helping the user operate the live runtime from a shell window.\n\
You can read source files, list directories, search code, write files, replace text in files, run shell commands (cargo build/test/check), inspect runtime status, list plugins/nodes, invoke plugins, execute targets, and reload the runtime.\n\
\n\
QQ GROUP CHAT MODE — when you receive messages forwarded from a QQ group:\n\
- You are running in a QQ group. Messages may be casual chat NOT directed at you.\n\
- CRITICAL: Always decide whether the message is actually talking to YOU before responding.\n\
- A message is directed at you if:\n\
  - Someone explicitly mentions your name, \"机器人\", \"bot\", or \"Cordis\"\n\
  - Someone asks a direct question (even without @mention)\n\
  - Someone gives a clear command or instruction\n\
  - The message has a question word (how, why, what, when, where, 怎么, 为什么, 如何, 帮我)\n\
- A message is NOT directed at you if:\n\
  - It's casual chat between group members\n\
  - It's an emoji, sticker, or single-word reply\n\
  - It's someone talking about another person/topic without involving you\n\
  - It's a statement not asking for anything\n\
- If the message is NOT directed at you, reply with EXACTLY: IGNORE\n\
  (this single word, nothing else — the caller will skip it)\n\
- If the message IS directed at you, reply normally — be concise and helpful.\n\
  Your reply will be sent directly to the QQ group.\n\
- To send a proactive notification to a QQ group, use:\n\
  invoke_plugin(qq, qq_send, {\"node_id\":\"qq_send\",\"target\":\"group:<id>\",\"message\":\"<text>\"})\n\
\n\
SAFETY RULES — never do these without explicit user request:\n\
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
- When creating NEW files/directories under plugins/, use `run_command` with shell commands\n\
  (e.g. `mkdir -p plugins/expr/evaluator/pow/src`) first — write_file may reject non-existent paths.\n\
- After a successful `cargo build`, the built .so files need to be synced to `artifacts/`\n\
  for the runtime to pick them up:\n\
  `cp plugins/target/debug/libexpr*.so artifacts/ 2>/dev/null; cp plugins/target/debug/*.so artifacts/ 2>/dev/null`\n\
  Then use `reload_runtime` to load the changes into the live runtime.\n\
\n\
When the user asks you to add a feature or fix a bug, follow this workflow:\n\
1. Read the relevant source files to understand the codebase structure.\n\
2. Plan the edits needed (which files to create/modify).\n\
3. Make the edits using write_file and replace_in_file (and run_command for new files/dirs).\n\
4. Run `cd plugins && cargo build 2>&1` to verify compilation.\n\
5. Run `cd plugins && cargo test -p <name> 2>&1` to verify correctness.\n\
6. Sync artifacts and reload for runtime changes (if needed).\n\
7. If something goes wrong, use revert_changes to undo, then try a different approach.\n\
Always verify edits compile before claiming success. If a command fails, read the error output and fix it.\n\
Prefer concise, operator-friendly replies. Mention important tool outcomes plainly.\n\
Do not invent runtime state or claim a command succeeded unless a tool confirmed it."
}

fn resolve_api_key(config: &LlmApiConfig) -> Result<String, RuntimeError> {
    if let Some(api_key) = &config.api_key {
        if !api_key.trim().is_empty() {
            return Ok(api_key.clone());
        }
    }
    if api_key_env_looks_like_secret(&config.api_key_env) {
        return Err(RuntimeError::InvalidArgument {
            message: "llm_api.api_key_env must be an environment variable name like OPENAI_API_KEY, not the API key value itself".to_string(),
        });
    }
    env::var(&config.api_key_env).map_err(|_| RuntimeError::MissingLlmApiKey {
        env_name: config.api_key_env.clone(),
    })
}

fn api_key_env_looks_like_secret(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("sk-") || trimmed.starts_with("sk_")
}

fn emit_agent_diagnostic(message: String) {
    if env::var(LLM_DEBUG_ENV).ok().as_deref() == Some("1") {
        eprintln!("{message}");
    }
}

fn summarize_request(endpoint: &str, request_body: &Value, timeout_ms: u64) -> String {
    let model = request_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let messages = request_body
        .get("messages")
        .and_then(Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    let tools = request_body
        .get("tools")
        .and_then(Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0);
    let tool_choice = request_body
        .get("tool_choice")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let stream = request_body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    format!(
        "endpoint={} model={} timeout_ms={} messages={} tools={} tool_choice={} stream={}",
        endpoint, model, timeout_ms, messages, tools, tool_choice, stream
    )
}

fn format_transport_error(err: &reqwest::Error, timeout_ms: u64) -> String {
    if err.is_timeout() {
        format!("request timed out after timeout_ms={timeout_ms}: {err}")
    } else {
        err.to_string()
    }
}

fn format_stream_error(err: &std::io::Error, timeout_ms: u64) -> String {
    if err.kind() == std::io::ErrorKind::TimedOut {
        format!("stream read timed out after timeout_ms={timeout_ms}: {err}")
    } else {
        err.to_string()
    }
}

fn extract_error_message(raw_body: &str) -> Option<String> {
    let json: Value = serde_json::from_str(raw_body).ok()?;
    json.get("error")
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            json.get("detail")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

fn truncate_for_error(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn normalize_streamed_optional_text(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn merge_stream_field(target: &mut String, delta: &str, append: bool) {
    if append {
        target.push_str(delta);
    } else if target.is_empty() {
        target.push_str(delta);
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
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    const TEST_TOOL_RECORD: &str = "record_summary";
    const TEST_TOOL_READ: &str = "read_context";

    #[derive(Default)]
    struct FakeHost;

    #[derive(Default)]
    struct TerminalTestBackend {
        executed_tools: Vec<String>,
    }

    impl AgentBackend for TerminalTestBackend {
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

        fn agent_reload_runtime(&self) -> Result<Value, RuntimeError> {
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
    }

    #[test]
    fn reasoning_only_request_message_uses_empty_content() {
        let message = ChatMessage {
            content: None,
            reasoning_content: Some("Need another reasoning pass".to_string()),
            tool_calls: Vec::new(),
        };
        let request = message.to_request_message();
        assert_eq!(request.get("role").and_then(Value::as_str), Some("assistant"));
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

    #[test]
    fn shell_agent_uses_runtime_tool_and_keeps_history() {
        let first_response = sse_response(vec![
            json!({
                "id": "chatcmpl_agent_1",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_runtime_status",
                            "type": "function",
                            "function": {
                                "name": AGENT_TOOL_GET_RUNTIME_STATUS,
                                "arguments": "{}"
                            }
                        }]
                    }
                }]
            }),
            json!({
                "id": "chatcmpl_agent_1",
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            }),
        ]);
        let second_response = sse_response(vec![
            json!({
                "id": "chatcmpl_agent_2",
                "choices": [{
                    "delta": {
                        "content": "Runtime is healthy and loaded."
                    }
                }]
            }),
            json!({
                "id": "chatcmpl_agent_2",
                "choices": [{
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
        ]);
        let (base_url, requests_rx, handle) =
            spawn_chunked_mock_llm_server_sequence(vec![first_response, second_response]);

        let config = LlmApiConfig {
            provider: "deepseek".to_string(),
            base_url,
            api_key: Some("test-key".to_string()),
            model: "deepseek-reasoner".to_string(),
            timeout_ms: 30_000,
            ..LlmApiConfig::default()
        };
        let mut session = ShellAgentSession::new(config).expect("build session");
        let reply = session
            .respond(&FakeHost, "What is the runtime status right now?")
            .expect("agent reply");

        assert_eq!(reply.content, "Runtime is healthy and loaded.");
        assert_eq!(reply.tool_events.len(), 1);
        assert_eq!(reply.tool_events[0].name, AGENT_TOOL_GET_RUNTIME_STATUS);
        assert_eq!(session.status().completed_turns, 1);

        let requests = requests_rx.recv().expect("captured requests");
        handle.join().expect("join mock server");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("\"tools\""));
        assert!(requests[1].contains("snapshot-demo"));
    }

    #[test]
    fn terminal_tool_reply_ends_agent_session_without_extra_turn() {
        let response = sse_response(vec![
            json!({
                "id": "chatcmpl_agent_terminal",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_record_summary",
                            "type": "function",
                            "function": {
                                "name": TEST_TOOL_RECORD,
                                "arguments": "{}"
                            }
                        }]
                    }
                }]
            }),
            json!({
                "id": "chatcmpl_agent_terminal",
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            }),
        ]);
        let (base_url, requests_rx, handle) =
            spawn_chunked_mock_llm_server_sequence(vec![response]);

        let config = LlmApiConfig {
            provider: "deepseek".to_string(),
            base_url,
            api_key: Some("test-key".to_string()),
            model: "deepseek-reasoner".to_string(),
            timeout_ms: 30_000,
            ..LlmApiConfig::default()
        };
        let mut session = AgentSession::new(config, "plugin_iteration").expect("build session");
        let mut backend = TerminalTestBackend::default();

        let reply = session
            .respond(&mut backend, "Finish the iteration")
            .expect("terminal tool should end session");

        let requests = requests_rx.recv().expect("captured requests");
        handle.join().expect("join mock server");

        assert_eq!(reply.content, "Terminal summary recorded.");
        assert_eq!(reply.tool_events.len(), 1);
        assert_eq!(backend.executed_tools, vec![TEST_TOOL_RECORD.to_string()]);
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn terminal_tool_must_be_last_tool_call_in_turn() {
        let response = sse_response(vec![
            json!({
                "id": "chatcmpl_agent_bad_terminal",
                "choices": [{
                    "delta": {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call_record_summary",
                                "type": "function",
                                "function": {
                                    "name": TEST_TOOL_RECORD,
                                    "arguments": "{}"
                                }
                            },
                            {
                                "index": 1,
                                "id": "call_read_context",
                                "type": "function",
                                "function": {
                                    "name": TEST_TOOL_READ,
                                    "arguments": "{}"
                                }
                            }
                        ]
                    }
                }]
            }),
            json!({
                "id": "chatcmpl_agent_bad_terminal",
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls"
                }]
            }),
        ]);
        let (base_url, _requests_rx, handle) =
            spawn_chunked_mock_llm_server_sequence(vec![response]);

        let config = LlmApiConfig {
            provider: "deepseek".to_string(),
            base_url,
            api_key: Some("test-key".to_string()),
            model: "deepseek-reasoner".to_string(),
            timeout_ms: 30_000,
            ..LlmApiConfig::default()
        };
        let mut session = AgentSession::new(config, "plugin_iteration").expect("build session");
        let mut backend = TerminalTestBackend::default();

        let err = session
            .respond(&mut backend, "Bad terminal ordering")
            .expect_err("terminal tool should be last");
        handle.join().expect("join mock server");

        assert!(err
            .to_string()
            .contains("terminal agent tool record_summary must be the last tool call"));
        assert_eq!(backend.executed_tools, vec![TEST_TOOL_RECORD.to_string()]);
    }

    fn spawn_chunked_mock_llm_server_sequence(
        responses: Vec<Vec<(u64, String)>>,
    ) -> (String, mpsc::Receiver<Vec<String>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("listener addr");
        let (sender, receiver) = mpsc::channel();

        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for chunks in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request = String::new();

                let mut first_line = String::new();
                reader
                    .read_line(&mut first_line)
                    .expect("read request line");
                request.push_str(&first_line);

                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read header line");
                    request.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                    let lowercase = line.to_ascii_lowercase();
                    if let Some(value) = lowercase.strip_prefix("content-length:") {
                        content_length = value.trim().parse::<usize>().expect("content length");
                    }
                }

                let mut body = vec![0_u8; content_length];
                reader.read_exact(&mut body).expect("read request body");
                request.push_str(&String::from_utf8_lossy(&body));
                requests.push(request);

                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                )
                .expect("write response headers");
                stream.flush().expect("flush response headers");

                for (delay_ms, chunk) in chunks {
                    thread::sleep(Duration::from_millis(delay_ms));
                    write!(stream, "{:X}\r\n{}\r\n", chunk.len(), chunk).expect("write chunk");
                    stream.flush().expect("flush chunk");
                }
                write!(stream, "0\r\n\r\n").expect("finish chunked response");
                stream.flush().expect("flush chunked end");
            }
            sender.send(requests).expect("send captured requests");
        });

        (format!("http://{}/v1", address), receiver, handle)
    }

    fn sse_response(events: Vec<Value>) -> Vec<(u64, String)> {
        let mut chunks = events
            .into_iter()
            .map(|event| (0, format!("data: {}\n\n", event)))
            .collect::<Vec<_>>();
        chunks.push((0, "data: [DONE]\n\n".to_string()));
        chunks
    }
}
