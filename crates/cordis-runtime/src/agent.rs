use crate::config::LlmApiConfig;
use crate::core::error::RuntimeError;
use crate::host::RuntimeHost;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::env;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::{Duration, Instant};

const AGENT_HISTORY_MESSAGE_LIMIT: usize = 24;
const AGENT_MAX_TOOL_TURNS: usize = 32;
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
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ShellAgentStatus {
    pub provider: String,
    pub model: String,
    pub completed_turns: usize,
    pub stored_messages: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ShellAgentReply {
    pub response_id: Option<String>,
    pub content: String,
    pub tool_events: Vec<AgentToolEvent>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentToolEvent {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ShellAgentSession {
    config: LlmApiConfig,
    client: Client,
    history: Vec<Value>,
}

impl ShellAgentSession {
    pub fn new(config: LlmApiConfig) -> Result<Self, RuntimeError> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|err| RuntimeError::LlmRequestFailed {
                message: format!("failed to build shell agent HTTP client: {err}"),
            })?;
        Ok(Self {
            config,
            client,
            history: Vec::new(),
        })
    }

    pub fn reset(&mut self) {
        self.history.clear();
    }

    pub fn status(&self) -> ShellAgentStatus {
        ShellAgentStatus {
            provider: self.config.provider.clone(),
            model: self.config.model.clone(),
            completed_turns: self.history.len() / 2,
            stored_messages: self.history.len(),
        }
    }

    pub fn respond<H: AgentToolHost + ?Sized>(
        &mut self,
        host: &H,
        user_input: &str,
    ) -> Result<ShellAgentReply, RuntimeError> {
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
            "content": shell_agent_system_prompt(),
        }));
        messages.extend(self.history.clone());
        messages.push(json!({
            "role": "user",
            "content": trimmed,
        }));

        let tools = shell_agent_tools();
        let turn_started = Instant::now();
        let mut tool_events = Vec::new();

        for turn in 0..AGENT_MAX_TOOL_TURNS {
            if turn_started.elapsed() >= Duration::from_millis(self.config.timeout_ms) {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!(
                        "shell agent exceeded total response budget after {} tool turns; elapsed_ms={} timeout_ms={}",
                        turn,
                        turn_started.elapsed().as_millis(),
                        self.config.timeout_ms,
                    ),
                });
            }

            emit_agent_diagnostic(format!(
                "agent_turn_start turn={} elapsed_ms={} messages={} tools={}",
                turn + 1,
                turn_started.elapsed().as_millis(),
                messages.len(),
                tools.len(),
            ));

            let request_body = json!({
                "model": self.config.model,
                "messages": messages,
                "temperature": self.config.temperature,
                "max_tokens": self.config.max_tokens,
                "tools": tools,
                "tool_choice": "auto",
            });
            let (message, response_id, finish_reason) =
                self.send_chat_request(endpoint.clone(), request_body)?;

            emit_agent_diagnostic(format!(
                "agent_turn_result turn={} response_id={} tool_calls={} content_chars={} reasoning_chars={} finish_reason={}",
                turn + 1,
                response_id.as_deref().unwrap_or("-"),
                message.tool_calls.len(),
                message.content.as_deref().map(str::len).unwrap_or(0),
                message.reasoning_content.as_deref().map(str::len).unwrap_or(0),
                finish_reason.as_deref().unwrap_or("-"),
            ));

            if !message.tool_calls.is_empty() {
                messages.push(message.to_request_message());
                for tool_call in &message.tool_calls {
                    let (event, tool_output) = execute_agent_tool_call(host, tool_call)?;
                    tool_events.push(event);
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call.id,
                        "content": tool_output,
                    }));
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
                return Ok(ShellAgentReply {
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
                message: "shell agent response had neither tool_calls nor final content"
                    .to_string(),
            });
        }

        Err(RuntimeError::LlmResponseInvalid {
            message: format!(
                "shell agent exceeded safety turn limit {} without producing a final response",
                AGENT_MAX_TOOL_TURNS
            ),
        })
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
        message.insert(
            "content".to_string(),
            self.content
                .as_ref()
                .map(|content| Value::String(content.clone()))
                .unwrap_or(Value::Null),
        );
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

fn execute_agent_tool_call<H: AgentToolHost + ?Sized>(
    host: &H,
    tool_call: &ToolCall,
) -> Result<(AgentToolEvent, String), RuntimeError> {
    let args_json = if tool_call.function.arguments.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<Value>(&tool_call.function.arguments).map_err(|err| {
            RuntimeError::LlmResponseInvalid {
                message: format!(
                    "shell agent tool {} received invalid JSON arguments: {err}",
                    tool_call.function.name
                ),
            }
        })?
    };

    let output = match tool_call.function.name.as_str() {
        AGENT_TOOL_GET_RUNTIME_STATUS => {
            parse_tool_arguments::<EmptyArgs>(
                &tool_call.function.arguments,
                &tool_call.function.name,
            )?;
            host.agent_runtime_status()?
        }
        AGENT_TOOL_LIST_PLUGINS => {
            parse_tool_arguments::<EmptyArgs>(
                &tool_call.function.arguments,
                &tool_call.function.name,
            )?;
            host.agent_list_plugins()?
        }
        AGENT_TOOL_LIST_NODES => {
            parse_tool_arguments::<EmptyArgs>(
                &tool_call.function.arguments,
                &tool_call.function.name,
            )?;
            host.agent_list_nodes()?
        }
        AGENT_TOOL_GET_KERNEL_STATUS => {
            parse_tool_arguments::<EmptyArgs>(
                &tool_call.function.arguments,
                &tool_call.function.name,
            )?;
            host.agent_kernel_status()?
        }
        AGENT_TOOL_GET_KERNEL_ISSUES => {
            parse_tool_arguments::<EmptyArgs>(
                &tool_call.function.arguments,
                &tool_call.function.name,
            )?;
            host.agent_kernel_issues()?
        }
        AGENT_TOOL_RELOAD_RUNTIME => {
            parse_tool_arguments::<EmptyArgs>(
                &tool_call.function.arguments,
                &tool_call.function.name,
            )?;
            host.agent_reload_runtime()?
        }
        AGENT_TOOL_INVOKE_PLUGIN => {
            let args = parse_tool_arguments::<InvokePluginArgs>(
                &tool_call.function.arguments,
                &tool_call.function.name,
            )?;
            host.agent_invoke_plugin(&args.plugin_path, &args.node_id, args.payload_json)?
        }
        AGENT_TOOL_EXECUTE_TARGET => {
            let args = parse_tool_arguments::<ExecuteTargetArgs>(
                &tool_call.function.arguments,
                &tool_call.function.name,
            )?;
            host.agent_execute_target(&args.node_fqn, args.payload_json)?
        }
        other => {
            return Err(RuntimeError::LlmResponseInvalid {
                message: format!("shell agent returned unsupported tool call: {other}"),
            });
        }
    };

    Ok((
        AgentToolEvent {
            name: tool_call.function.name.clone(),
            arguments: args_json,
        },
        output.to_string(),
    ))
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
            continue;
        }

        if let Some(data) = trimmed.strip_prefix("data:") {
            pending_data_lines.push(data.trim_start().to_string());
        }
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

fn shell_agent_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": AGENT_TOOL_GET_RUNTIME_STATUS,
                "description": "Get the current runtime host status, snapshot ids, candidate status, and recent reload reports.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": AGENT_TOOL_LIST_PLUGINS,
                "description": "List currently registered plugins, their load status, parent relationship, and known node ids.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": AGENT_TOOL_LIST_NODES,
                "description": "List currently registered node FQNs so you can choose a valid execute target.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": AGENT_TOOL_GET_KERNEL_STATUS,
                "description": "Get kernel status including plugin issue counts and blocked iteration counts.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": AGENT_TOOL_GET_KERNEL_ISSUES,
                "description": "List observed kernel plugin issues that may require iteration or reload investigation.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": AGENT_TOOL_RELOAD_RUNTIME,
                "description": "Reload the runtime snapshot and return the full reload diagnostics report.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": AGENT_TOOL_INVOKE_PLUGIN,
                "description": "Invoke a plugin node directly by plugin path and node id.",
                "parameters": {
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
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": AGENT_TOOL_EXECUTE_TARGET,
                "description": "Execute a registered node target through the runtime execution engine.",
                "parameters": {
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
                },
            },
        }),
    ]
}

fn shell_agent_system_prompt() -> &'static str {
    "You are the Cordis shell agent running inside the cordis-runtime serve REPL.\n\
You are helping the user operate the live runtime from a shell window.\n\
Use tools whenever the user asks about current runtime state, plugin status, kernel issues, reload behavior, or asks you to run something.\n\
Prefer concise, operator-friendly replies. Mention important tool outcomes plainly.\n\
Do not invent runtime state or claim a command succeeded unless a tool confirmed it.\n\
If the user asks for an action that your tools cannot perform, say that clearly and explain the closest supported action."
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

fn parse_tool_arguments<T>(raw_args: &str, tool_name: &str) -> Result<T, RuntimeError>
where
    T: DeserializeOwned,
{
    let normalized = if raw_args.trim().is_empty() {
        "{}"
    } else {
        raw_args
    };
    serde_json::from_str::<T>(normalized).map_err(|err| RuntimeError::LlmResponseInvalid {
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
