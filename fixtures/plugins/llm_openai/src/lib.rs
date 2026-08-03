//! OpenAI 兼容的 LLM provider 插件。
//!
//! 本插件承载**全部**供应商专有的传输细节：`/chat/completions` 端点拼装、
//! Bearer 鉴权与 `OpenAI-Organization`/`OpenAI-Project` 头、5xx/超时重试、
//! SSE 增量解析与 tool_call 分片重组。kernel 侧只保留 agent 循环（轮次管理、
//! 工具分发、历史压缩），把构造好的请求体原样交过来。
//!
//! 这条边界让"换 provider"变成"写一个插件"。（拆分前配置里虽有
//! `provider: openai|deepseek`，但全仓没有一处按它分支 wire format——所谓
//! DeepSeek 支持就是同一套 OpenAI 格式换个 base_url。）
//!
//! # 节点
//!
//! `llm_complete` — 执行一次补全，请求/回复形状见 `cordis_plugin_sdk::llm`。
//!
//! # token 流式旁路
//!
//! 请求带 `sink_addr` 时，插件边读上游 SSE 边把增量逐帧写到该 TCP 地址
//! （行分隔 JSON，首帧 `{"key":...}` 握手）。**纯旁路**：连不上、写失败或
//! 整个忽略都不影响结果——补全始终完整地由本次调用的返回值给出。
//!
//! 不用 dlsym 回调的原因：`dlsym(RTLD_DEFAULT)` 在测试二进制与 macOS 下拿不到
//! 宿主符号（SDK 自己的测试已记录），且其 C 签名无法区分并发会话。
//!
//! 注意传输层**不再直接打印**任何东西。拆分前逐 token 的 stdout 输出内联在
//! SSE 读取里，导致 inbox/serve 路径无法关掉流式；现在增量只推给 sink，
//! 由 host 决定展示与否。

use cordis_plugin_sdk::llm::{
    LlmCompletionRequest, LlmMessage, LlmStreamFrame, LlmToolCall, LlmToolFunction,
    LlmTransportConfig,
};
use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginDocs,
    PluginRequest, PluginResponse,
};
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// 单次补全的最大尝试次数（沿用拆分前 kernel 侧常量）。
const AGENT_REQUEST_MAX_ATTEMPTS: usize = 3;
/// 两次尝试之间的固定退避。
const AGENT_REQUEST_RETRY_BACKOFF_MS: u64 = 500;

/// 诊断输出。kernel 侧走 `emit_agent_diagnostic`；插件里简化为受同一环境变量
/// 控制的 stderr，避免把 host 内部 API 拖进 ABI。
fn diag(message: String) {
    if std::env::var("CORDIS_AGENT_DIAG").is_ok() {
        eprintln!("[llm_openai] {message}");
    }
}

// ---------------------------------------------------------------------------
// token 流式旁路
// ---------------------------------------------------------------------------

/// 往 host 的 sink 地址写增量帧。
///
/// 任何写失败都只记一次诊断并**永久放弃**这条旁路（置空句柄），不再每帧重试
/// 拖慢补全——sink 坏掉不能影响正事。
struct SinkWriter {
    stream: RefCell<Option<TcpStream>>,
}

impl SinkWriter {
    /// 连接并握手；任一步失败返回 `None`，调用方据此降级为非流式。
    fn connect(addr: &str, key: &str) -> Option<Self> {
        let stream = match TcpStream::connect(addr) {
            Ok(stream) => stream,
            Err(err) => {
                diag(format!("sink connect failed addr={addr}: {err}"));
                return None;
            }
        };
        let writer = Self {
            stream: RefCell::new(Some(stream)),
        };
        writer.send(&LlmStreamFrame::Key {
            key: key.to_string(),
        });
        Some(writer)
    }

    fn send(&self, frame: &LlmStreamFrame) {
        let mut guard = self.stream.borrow_mut();
        let Some(stream) = guard.as_mut() else {
            return;
        };
        let Ok(mut line) = serde_json::to_string(frame) else {
            return;
        };
        line.push('\n');
        if let Err(err) = stream
            .write_all(line.as_bytes())
            .and_then(|()| stream.flush())
        {
            diag(format!("sink write failed, dropping side-channel: {err}"));
            *guard = None;
        }
    }

    fn reasoning(&self, delta: &str) {
        if !delta.is_empty() {
            self.send(&LlmStreamFrame::Reasoning {
                d: delta.to_string(),
            });
        }
    }

    fn content(&self, delta: &str) {
        if !delta.is_empty() {
            self.send(&LlmStreamFrame::Content {
                d: delta.to_string(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI SSE wire 结构（插件私有：这些形状不进 kernel↔plugin 契约）
// ---------------------------------------------------------------------------

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
    message: LlmMessage,
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
    fn apply_chunk(&mut self, chunk: ChatChunk) -> Result<StreamEventSummary, String> {
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

    fn finish(self) -> Result<(LlmMessage, Option<String>), String> {
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
                    return Err(format!(
                            "streamed shell agent tool call was incomplete: id_present={} type_present={} name_present={} arguments_present={}",
                            !tool.id.is_empty(),
                            !tool.call_type.is_empty(),
                            !tool.name.is_empty(),
                            !tool.arguments.is_empty(),
                        ));
                }
                Ok(LlmToolCall {
                    id: tool.id,
                    call_type: tool.call_type,
                    function: LlmToolFunction {
                        name: tool.name,
                        arguments: tool.arguments,
                    },
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok((
            LlmMessage {
                content: normalize_streamed_optional_text(self.content),
                reasoning_content: normalize_streamed_optional_text(self.reasoning_content),
                tool_calls,
            },
            self.response_id,
        ))
    }
}

// ---------------------------------------------------------------------------
// HTTP 传输与 SSE 读取
// ---------------------------------------------------------------------------

fn send_chat_request(
    client: &Client,
    cfg: &LlmTransportConfig,
    endpoint: String,
    mut request_body: Value,
    sink: Option<&SinkWriter>,
) -> Result<(LlmMessage, Option<String>, Option<String>), String> {
    request_body["stream"] = Value::Bool(true);
    let api_key = resolve_api_key(cfg)?;
    let request_summary = summarize_request(&endpoint, &request_body, cfg.timeout_ms);
    let overall_started = Instant::now();

    diag(format!(
        "agent_request_start attempts={} {}",
        AGENT_REQUEST_MAX_ATTEMPTS, request_summary
    ));

    for attempt in 1..=AGENT_REQUEST_MAX_ATTEMPTS {
        let attempt_started = Instant::now();
        let mut http_request = client
            .post(endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, format!("Bearer {api_key}"));
        if let Some(org) = &cfg.organization {
            if !org.trim().is_empty() {
                http_request = http_request.header("OpenAI-Organization", org);
            }
        }
        if let Some(project) = &cfg.project {
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
                    format_transport_error(&err, cfg.timeout_ms),
                );
                if attempt < AGENT_REQUEST_MAX_ATTEMPTS {
                    diag(format!(
                        "{message} retry_backoff_ms={AGENT_REQUEST_RETRY_BACKOFF_MS}"
                    ));
                    thread::sleep(Duration::from_millis(AGENT_REQUEST_RETRY_BACKOFF_MS));
                    continue;
                }
                return Err(message);
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
                        format_transport_error(&err, cfg.timeout_ms),
                    );
                    if attempt < AGENT_REQUEST_MAX_ATTEMPTS {
                        diag(format!(
                            "{message} retry_backoff_ms={AGENT_REQUEST_RETRY_BACKOFF_MS}"
                        ));
                        thread::sleep(Duration::from_millis(AGENT_REQUEST_RETRY_BACKOFF_MS));
                        continue;
                    }
                    return Err(message);
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
            if attempt < AGENT_REQUEST_MAX_ATTEMPTS {
                // P1-35: don't retry permanent-failure client errors
                // (4xx except 408 / 429). Bad API key, invalid model,
                // quota exhausted, permission denied — retrying just
                // burns network round trips and can trigger rate-
                // limit escalation.
                let code = status.as_u16();
                let is_permanent_4xx = (400..500).contains(&code) && code != 408 && code != 429;
                if is_permanent_4xx {
                    diag(format!("{message} not-retrying (permanent 4xx)"));
                    return Err(message);
                }
                diag(format!(
                    "{message} retry_backoff_ms={AGENT_REQUEST_RETRY_BACKOFF_MS}"
                ));
                thread::sleep(Duration::from_millis(AGENT_REQUEST_RETRY_BACKOFF_MS));
                continue;
            }
            return Err(message);
        }

        let stream_timeout = Duration::from_secs(cfg.stream_timeout_secs);
        let streamed = match read_chat_stream(
            response,
            sink,
            &request_summary,
            attempt,
            stream_timeout,
        ) {
            Ok(streamed) => streamed,
            Err(ChatStreamReadError::Io(err)) => {
                let message = format!(
                    "shell agent request failed: attempt={attempt}/{AGENT_REQUEST_MAX_ATTEMPTS} phase=read_stream elapsed_ms={} total_elapsed_ms={} {} detail={}",
                    attempt_started.elapsed().as_millis(),
                    overall_started.elapsed().as_millis(),
                    request_summary,
                    format_stream_error(&err, cfg.timeout_ms),
                );
                if attempt < AGENT_REQUEST_MAX_ATTEMPTS {
                    diag(format!(
                        "{message} retry_backoff_ms={AGENT_REQUEST_RETRY_BACKOFF_MS}"
                    ));
                    thread::sleep(Duration::from_millis(AGENT_REQUEST_RETRY_BACKOFF_MS));
                    continue;
                }
                return Err(message);
            }
            Err(ChatStreamReadError::InvalidResponse(message)) => {
                return Err(message);
            }
        };

        diag(format!(
            "agent_request_success attempt={attempt}/{AGENT_REQUEST_MAX_ATTEMPTS} status={} elapsed_ms={} total_elapsed_ms={} response_bytes={} stream_events={} stream_done={} content_chars={} finish_reason={} {}",
            status.as_u16(),
            attempt_started.elapsed().as_millis(),
            overall_started.elapsed().as_millis(),
            streamed.raw_bytes,
            streamed.event_count,
            streamed.saw_done,
            streamed.message.content.as_ref().map(|c| c.len()).unwrap_or(0),
            streamed.finish_reason.as_deref().unwrap_or("-"),
            request_summary,
        ));

        return Ok((
            streamed.message,
            streamed.response_id,
            streamed.finish_reason,
        ));
    }

    Err(format!(
        "shell agent request exhausted retries without returning a streamed response: {}",
        request_summary
    ))
}

fn read_chat_stream(
    response: Response,
    sink: Option<&SinkWriter>,
    request_summary: &str,
    attempt: usize,
    timeout: Duration,
) -> Result<ChatStreamReadResult, ChatStreamReadError> {
    // Channel capacity: 8 chunks of 8 KiB each — enough to smooth out
    // network jitter without buffering the entire response in memory.
    let (tx, rx) = sync_channel::<std::io::Result<Vec<u8>>>(8);

    // Read the response body in a background thread so the main loop can
    // enforce a per-read timeout via recv_timeout.  This prevents the
    // agent from hanging indefinitely when the server stalls mid-stream.
    //
    // P1-34: `reader.read()` blocks on the underlying TCP socket, and the
    // reader-thread has no explicit stop signal — if the main loop
    // returns early (e.g. InvalidResponse), the thread would only exit
    // when the next `read()` completes. That is bounded by the
    // reqwest::Client `timeout(...)` set at construction (see
    // `AgentSession::new` / `from_snapshot`), which caps the total time
    // a `read()` can block. When the client timeout eventually fires,
    // the thread hits an Io error, sees `tx.send(Err(e))` fail (rx has
    // been dropped) and exits. Not zero-cost — one temporarily-alive
    // reader thread per failed attempt — but bounded and self-cleaning.
    let _reader_thread = thread::spawn(move || {
        let mut reader = BufReader::new(response);
        loop {
            let mut buf = vec![0u8; 8192];
            match reader.read(&mut buf) {
                Ok(0) => {
                    // EOF — send empty marker.
                    let _ = tx.send(Ok(Vec::new()));
                    break;
                }
                Ok(n) => {
                    buf.truncate(n);
                    if tx.send(Ok(buf)).is_err() {
                        break; // receiver hung up (timeout / error)
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    });

    let mut raw_bytes = 0usize;
    let mut event_count = 0usize;
    let mut saw_done = false;
    let mut finish_reason = None;
    let mut pending_data_lines = Vec::new();
    let mut accumulator = ChatMessageAccumulator::default();
    let mut flushed_content_len = 0usize;
    let mut flushed_reasoning_len = 0usize;

    // Line buffer: accumulated raw bytes that haven't yielded a complete line yet.
    let mut line_buf: Vec<u8> = Vec::new();

    /// Drain complete lines from `line_buf`, returning each terminated line
    /// (including the `\n`) as a String.  Leftover bytes (incomplete line)
    /// stay in `line_buf`.
    fn drain_lines(line_buf: &mut Vec<u8>) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = line_buf.drain(..=pos).collect();
            lines.push(String::from_utf8_lossy(&line_bytes).to_string());
        }
        lines
    }

    let stream_started = Instant::now();
    let overall_deadline = timeout * 5;

    loop {
        // Overall deadline: if the stream takes way longer than expected,
        // the server is hung — don't wait for per-chunk timeouts.
        if stream_started.elapsed() > overall_deadline {
            return Err(ChatStreamReadError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "stream overall deadline exceeded after {:?} (received {} bytes, {} events, timeout {:?})",
                    overall_deadline, raw_bytes, event_count, timeout,
                ),
            )));
        }

        // Wait for the next chunk with a timeout.  Each chunk gets its own
        // timeout window so we don't fail just because the total response
        // takes longer than the budget — we only fail when the server goes
        // silent for too long.
        let chunk = match rx.recv_timeout(timeout) {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(e)) => return Err(ChatStreamReadError::Io(e)),
            Err(RecvTimeoutError::Timeout) => {
                return Err(ChatStreamReadError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "stream read timed out after {:?} (received {} bytes, {} events so far, elapsed {:?})",
                        timeout,
                        raw_bytes,
                        event_count,
                        stream_started.elapsed(),
                    ),
                )));
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Reader thread exited — flush any remaining data then break.
                if !line_buf.is_empty() {
                    // Treat leftover bytes as a final (incomplete) line.
                    let leftover = std::mem::take(&mut line_buf);
                    let line = String::from_utf8_lossy(&leftover).to_string();
                    raw_bytes += line.len();
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
                    // Also try to process the leftover as its own event.
                    if !line.trim().is_empty() {
                        if let Some(data) = line.trim().strip_prefix("data:") {
                            pending_data_lines.push(data.trim_start().to_string());
                        }
                    }
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
                }
                break;
            }
        };

        if chunk.is_empty() {
            // EOF from reader thread.
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

        raw_bytes += chunk.len();
        line_buf.extend_from_slice(&chunk);

        for line in drain_lines(&mut line_buf) {
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
                // Stream new reasoning — prefix every line with 💭.
                // 增量推给 sink 而不是直接打印：展示策略（REPL 逐字输出 /
                // inbox 静默累积）现在由 host 决定，传输层不再越权。
                let new_reasoning = &accumulator.reasoning_content[flushed_reasoning_len..];
                if !new_reasoning.is_empty() {
                    if let Some(sink) = sink {
                        sink.reasoning(new_reasoning);
                    }
                    flushed_reasoning_len = accumulator.reasoning_content.len();
                }
                let new_content = &accumulator.content[flushed_content_len..];
                if !new_content.is_empty() {
                    if let Some(sink) = sink {
                        sink.content(new_content);
                    }
                    flushed_content_len = accumulator.content.len();
                }
                continue;
            }

            if let Some(data) = trimmed.strip_prefix("data:") {
                pending_data_lines.push(data.trim_start().to_string());
            }
        }

        if saw_done {
            break;
        }
    }

    // 收尾：把最后一段尚未推送的增量补给 sink。
    let new_reasoning = &accumulator.reasoning_content[flushed_reasoning_len..];
    if !new_reasoning.is_empty() {
        if let Some(sink) = sink {
            sink.reasoning(new_reasoning);
        }
    }
    let new_content = &accumulator.content[flushed_content_len..];
    if !new_content.is_empty() {
        if let Some(sink) = sink {
            sink.content(new_content);
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
        diag(format!(
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
    diag(format!(
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

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 从环境变量取 API key。
///
/// 契约类型 `LlmTransportConfig` **刻意不携带明文 key**——它会被序列化进
/// 插件调用的 payload，带上密钥等于把它写进日志与崩溃快照的风险面。因此
/// 这里只认环境变量名，key 在请求时才从进程环境读取。
fn resolve_api_key(config: &LlmTransportConfig) -> Result<String, String> {
    if api_key_env_looks_like_secret(&config.api_key_env) {
        return Err("api_key_env must be an environment variable NAME like OPENAI_API_KEY, not the key value itself".to_string());
    }
    std::env::var(&config.api_key_env).map_err(|_| {
        format!(
            "LLM API key missing: set {} in the environment",
            config.api_key_env
        )
    })
}

fn api_key_env_looks_like_secret(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("sk-") || trimmed.starts_with("sk_")
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
    if append || target.is_empty() {
        target.push_str(delta);
    }
}

// ---------------------------------------------------------------------------
// 节点分派
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NodeRequest {
    #[serde(default)]
    node_id: Option<String>,
    #[serde(flatten)]
    completion: Option<LlmCompletionRequest>,
}

fn handle_llm_complete(req: LlmCompletionRequest) -> Result<Value, String> {
    let endpoint = format!(
        "{}/chat/completions",
        req.transport.base_url.trim_end_matches('/')
    );
    let client = Client::builder()
        .timeout(Duration::from_millis(req.transport.timeout_ms))
        .build()
        .map_err(|err| format!("failed to build http client: {err}"))?;

    let sink = match (req.sink_addr.as_deref(), req.sink_key.as_deref()) {
        (Some(addr), Some(key)) => SinkWriter::connect(addr, key),
        _ => None,
    };

    let (message, response_id, finish_reason) =
        send_chat_request(&client, &req.transport, endpoint, req.body, sink.as_ref())?;

    Ok(json!({
        "ok": true,
        "node_id": "llm_complete",
        "message": message,
        "response_id": response_id,
        "finish_reason": finish_reason,
    }))
}

fn handle(req: NodeRequest) -> Result<Value, String> {
    match req.node_id.as_deref() {
        Some("llm_complete") => {
            let completion = req
                .completion
                .ok_or_else(|| "llm_complete requires body/transport fields".to_string())?;
            handle_llm_complete(completion)
        }
        other => Err(format!("unknown llm_openai node_id: {other:?}")),
    }
}

fn api_handle(request: PluginRequest) -> PluginResponse {
    let result = match serde_json::from_str::<NodeRequest>(&request.payload) {
        Ok(req) => handle(req),
        Err(err) => Err(format!("invalid llm_openai request payload: {err}")),
    };
    match result {
        Ok(value) => json_response(&value),
        Err(error) => json_response(&json!({"ok": false, "error": error})),
    }
}

/// 集成测试入口：直接喂 payload 字符串、拿回复字符串，绕过 dylib 加载。
///
/// 传输测试关心的是 HTTP/SSE 行为，不该为此走一遍 dlopen 与工件索引。
#[doc(hidden)]
pub fn __test_handle(payload: String) -> String {
    api_handle(PluginRequest { payload }).payload
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_llm_openai_v1", "api_v2")
}

fn docs_value() -> PluginDocs {
    plugin_docs(
        "llm_openai",
        "llm_openai",
        "0.1.0",
        None,
        vec![node_doc(
            "llm_complete",
            "Execute one chat completion against an OpenAI-compatible endpoint.",
            json!({
                "type": "object",
                "required": ["node_id", "body", "transport"],
                "properties": {
                    "node_id": {"const": "llm_complete"},
                    "body": {"type": "object"},
                    "transport": {"type": "object"},
                    "sink_addr": {"type": "string"},
                    "sink_key": {"type": "string"}
                }
            }),
            json!({
                "type": "object",
                "required": ["ok"],
                "properties": {
                    "ok": {"type": "boolean"},
                    "message": {"type": "object"},
                    "response_id": {"type": ["string", "null"]},
                    "finish_reason": {"type": ["string", "null"]},
                    "error": {"type": "string"}
                }
            }),
            &["performs an outbound HTTPS request to the configured endpoint"],
            &[
                "upstream 5xx or timeout after retries",
                "missing or empty API key env var",
                "malformed SSE stream",
            ],
        )],
        None,
    )
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
