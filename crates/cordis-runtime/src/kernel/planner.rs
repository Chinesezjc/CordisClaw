//! LLM-backed patch planner for guarded auto-update workflows.

use crate::config::LlmApiConfig;
use crate::core::error::RuntimeError;
use crate::core::models::PluginDocs;
use crate::kernel::auto_update::{AutoUpdatePlan, FilePatch};
use crate::kernel::plugin_iteration::{
    normalize_rel_path as normalize_plugin_rel_path, PluginEditOpKind, PluginEditOperation,
    PluginEditPlan,
};
use crate::plugin::invoke::PluginInvoker;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const PLAN_SCHEMA_NAME: &str = "cordis_auto_update_plan";
const PLUGIN_EDIT_PLAN_SCHEMA_NAME: &str = "cordis_plugin_edit_plan";
// Keep a generous hard ceiling as a safety net, but prefer wall-clock budgeting.
const DEEPSEEK_REASONER_MAX_TURNS_SAFETY: usize = 128;
const LLM_REQUEST_MAX_ATTEMPTS: usize = 3;
const LLM_REQUEST_RETRY_BACKOFF_MS: u64 = 500;
const DEEPSEEK_REPEAT_READ_STRATEGY_THRESHOLD: usize = 2;
const DEEPSEEK_TOOL_LIST_CONTEXT_FILES: &str = "list_context_files";
const DEEPSEEK_TOOL_READ_CONTEXT_FILES: &str = "read_context_files";
const DEEPSEEK_TOOL_READ_CONTEXT_FILE: &str = "read_context_file";
const DEEPSEEK_TOOL_INSPECT_PLUGIN_CATALOG: &str = "inspect_plugin_catalog";
const DEEPSEEK_TOOL_SUBMIT_PATCH_PLAN: &str = "submit_patch_plan";
const DEEPSEEK_TOOL_SUBMIT_PLUGIN_EDIT_PLAN: &str = "submit_plugin_edit_plan";
const LLM_DEBUG_ENV: &str = "CORDIS_LLM_DEBUG";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanRequest {
    pub issue_id: String,
    pub patch_id: String,
    pub instruction: String,
    pub paths: Vec<String>,
    pub manual_approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedUpdate {
    pub plan: AutoUpdatePlan,
    pub summary: String,
    pub tests_command: Option<String>,
    pub safety_command: Option<String>,
    pub planner_model: String,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginPlanRequest {
    pub issue_id: String,
    pub patch_id: String,
    pub instruction: String,
    pub context_paths: Vec<String>,
    pub writable_roots: Vec<String>,
    pub manual_approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedPluginEdit {
    pub plan: PluginEditPlan,
    pub summary: String,
    pub tests_command: Option<String>,
    pub safety_command: Option<String>,
    pub planner_model: String,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct PlannerPayload {
    summary: String,
    #[serde(default)]
    tests_command: Option<String>,
    #[serde(default)]
    safety_command: Option<String>,
    patches: Vec<FilePatch>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct PluginPlannerPayload {
    summary: String,
    #[serde(default)]
    tests_command: Option<String>,
    #[serde(default)]
    safety_command: Option<String>,
    operations: Vec<PluginEditOperation>,
}

#[derive(Debug, Clone)]
struct ContextFile {
    path: String,
    sha256: String,
    content: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct PromptPluginCatalog {
    discovery_root: String,
    plugins: Vec<PromptPluginInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct PromptPluginInfo {
    plugin_path: String,
    plugin_id: String,
    plugin_version: String,
    command_name: Option<String>,
    nodes: Vec<PromptPluginNodeInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct PromptPluginNodeInfo {
    node_id: String,
    node_fqn: String,
    summary: String,
    input_schema: Value,
    output_schema: Value,
    side_effects: Vec<String>,
    failure_modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DeepSeekToolFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DeepSeekToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: DeepSeekToolFunctionCall,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
struct DeepSeekChatMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeepSeekToolCall>,
}

impl DeepSeekChatMessage {
    fn to_request_message(&self) -> Value {
        let mut message = Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        message.insert(
            "content".to_string(),
            self.content
                .as_ref()
                .map(|value| Value::String(value.clone()))
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
                serde_json::to_value(&self.tool_calls).unwrap_or_else(|_| Value::Array(Vec::new())),
            );
        }
        Value::Object(message)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
struct DeepSeekChatChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<DeepSeekChatChunkChoice>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
struct DeepSeekChatChunkChoice {
    #[serde(default)]
    delta: DeepSeekChatChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
struct DeepSeekChatChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeepSeekToolCallDelta>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
struct DeepSeekToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<DeepSeekToolFunctionCallDelta>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
struct DeepSeekToolFunctionCallDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct DeepSeekChatMessageAccumulator {
    response_id: Option<String>,
    content: String,
    reasoning_content: String,
    tool_calls: Vec<DeepSeekToolCallAccumulator>,
}

#[derive(Debug, Default)]
struct DeepSeekToolCallAccumulator {
    id: String,
    call_type: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct DeepSeekStreamEventSummary {
    delta_reasoning_chars: usize,
    delta_content_chars: usize,
    delta_tool_call_count: usize,
    finish_reason: Option<String>,
}

#[derive(Debug)]
struct DeepSeekChatStreamReadResult {
    response_id: Option<String>,
    message: DeepSeekChatMessage,
    raw_bytes: usize,
    event_count: usize,
    saw_done: bool,
    finish_reason: Option<String>,
}

#[derive(Debug)]
enum DeepSeekStreamReadError {
    Io(std::io::Error),
    InvalidResponse(String),
}

impl DeepSeekChatMessageAccumulator {
    fn apply_chunk(
        &mut self,
        chunk: DeepSeekChatChunk,
    ) -> Result<DeepSeekStreamEventSummary, RuntimeError> {
        if self.response_id.is_none() {
            self.response_id = chunk.id;
        }

        let mut summary = DeepSeekStreamEventSummary::default();
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
                    self.tool_calls.push(DeepSeekToolCallAccumulator::default());
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

    fn finish(self) -> Result<(DeepSeekChatMessage, Option<String>), RuntimeError> {
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
                            "streamed DeepSeek tool call was incomplete: id_present={} type_present={} name_present={} arguments_present={}",
                            !tool.id.is_empty(),
                            !tool.call_type.is_empty(),
                            !tool.name.is_empty(),
                            !tool.arguments.is_empty(),
                        ),
                    });
                }
                Ok(DeepSeekToolCall {
                    id: tool.id,
                    call_type: tool.call_type,
                    function: DeepSeekToolFunctionCall {
                        name: tool.name,
                        arguments: tool.arguments,
                    },
                })
            })
            .collect::<Result<Vec<_>, RuntimeError>>()?;

        Ok((
            DeepSeekChatMessage {
                content: normalize_streamed_optional_text(self.content),
                reasoning_content: normalize_streamed_optional_text(self.reasoning_content),
                tool_calls,
            },
            self.response_id,
        ))
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ReadContextFileArgs {
    path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ReadContextFilesArgs {
    paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
struct InspectPluginCatalogArgs {
    #[serde(default)]
    plugin_path: Option<String>,
}

enum DeepSeekToolOutcome<T> {
    ToolResult(String),
    Final(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeepSeekPlannerMode {
    Patch,
    Plugin,
}

#[derive(Debug, Default)]
struct DeepSeekReadInspectionState {
    seen_context_paths: BTreeSet<String>,
    consecutive_repeated_reads: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeepSeekReadObservation {
    requested_paths: Vec<String>,
    new_paths: Vec<String>,
    already_seen_paths: Vec<String>,
    consecutive_repeated_reads: usize,
}

impl DeepSeekReadInspectionState {
    fn record_read<'a, I>(&mut self, paths: I) -> DeepSeekReadObservation
    where
        I: IntoIterator<Item = &'a str>,
    {
        let requested_paths = paths
            .into_iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let (already_seen_paths, new_paths): (Vec<_>, Vec<_>) = requested_paths
            .iter()
            .cloned()
            .partition(|path| self.seen_context_paths.contains(path));

        if !requested_paths.is_empty() && new_paths.is_empty() {
            self.consecutive_repeated_reads += 1;
        } else {
            self.consecutive_repeated_reads = 0;
        }
        self.seen_context_paths
            .extend(requested_paths.iter().cloned());

        DeepSeekReadObservation {
            requested_paths,
            new_paths,
            already_seen_paths,
            consecutive_repeated_reads: self.consecutive_repeated_reads,
        }
    }
}

impl DeepSeekReadObservation {
    fn is_repeated_only(&self) -> bool {
        !self.requested_paths.is_empty() && self.new_paths.is_empty()
    }

    fn requires_change_strategy_hint(&self) -> bool {
        self.consecutive_repeated_reads >= DEEPSEEK_REPEAT_READ_STRATEGY_THRESHOLD
    }

    fn hint(&self, planner_mode: DeepSeekPlannerMode) -> Option<String> {
        if !self.is_repeated_only() {
            return None;
        }

        if self.requires_change_strategy_hint() {
            return Some(match planner_mode {
                DeepSeekPlannerMode::Patch => "You have already inspected these files in multiple consecutive turns. Before reading more context, decide your change strategy, then either call submit_patch_plan or inspect only one missing file that would change that strategy.".to_string(),
                DeepSeekPlannerMode::Plugin => "You have already inspected these files in multiple consecutive turns. Before reading more context, decide your change strategy (for example new_child_plugin, inline_core_patch, metadata_only, or docs_only), then either call submit_plugin_edit_plan or inspect only one missing file that would change that strategy.".to_string(),
            });
        }

        Some(format!(
            "These files were already inspected in previous turns: {}. Prefer reading only files you have not seen yet unless they would materially change the plan.",
            self.already_seen_paths.join(", ")
        ))
    }
}

#[derive(Debug, Clone)]
pub struct LlmPatchPlanner {
    config: LlmApiConfig,
    client: Client,
}

impl LlmPatchPlanner {
    pub fn new(config: LlmApiConfig) -> Result<Self, RuntimeError> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|err| RuntimeError::LlmRequestFailed {
                message: format!("failed to build HTTP client: {err}"),
            })?;
        Ok(Self { config, client })
    }

    pub fn plan(
        &self,
        workspace_root: &Path,
        request: PlanRequest,
    ) -> Result<PlannedUpdate, RuntimeError> {
        if request.instruction.trim().is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "instruction must not be empty".to_string(),
            });
        }
        if request.paths.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "at least one --path is required".to_string(),
            });
        }

        let allowed_paths = normalize_paths(&request.paths)?;
        let context_files = load_context_files(workspace_root, &allowed_paths)?;
        let plugin_catalog = discover_plugin_catalog(workspace_root);
        let provider = self.config.provider.trim().to_ascii_lowercase();
        let (payload, response_id) = match provider.as_str() {
            "openai" => self.plan_with_openai(&build_user_prompt(
                &request,
                &context_files,
                plugin_catalog.as_ref(),
            ))?,
            "deepseek" => {
                if model_uses_deepseek_reasoner(&self.config.model) {
                    let tool_prompt = build_deepseek_patch_tool_prompt(
                        &request,
                        &context_files,
                        plugin_catalog.as_ref(),
                    );
                    let fallback_prompt =
                        build_user_prompt(&request, &context_files, plugin_catalog.as_ref());
                    match self.plan_with_deepseek(
                        &tool_prompt,
                        &context_files,
                        plugin_catalog.as_ref(),
                    ) {
                        Ok(result) => result,
                        Err(reasoner_err) if should_fallback_deepseek_reasoner(&reasoner_err) => {
                            emit_llm_diagnostic(
                                true,
                                format!(
                                    "reasoner_fallback operation=patch_planning mode=one_shot_completion reason={}",
                                    reasoner_err
                                ),
                            );
                            self.plan_with_deepseek_completion(&fallback_prompt)
                                .map_err(|fallback_err| {
                                    combine_reasoner_fallback_errors(
                                        "patch planning",
                                        reasoner_err,
                                        fallback_err,
                                    )
                                })?
                        }
                        Err(err) => return Err(err),
                    }
                } else {
                    self.plan_with_deepseek(
                        &build_user_prompt(&request, &context_files, plugin_catalog.as_ref()),
                        &context_files,
                        plugin_catalog.as_ref(),
                    )?
                }
            }
            _ => {
                return Err(RuntimeError::UnsupportedLlmProvider {
                    provider: self.config.provider.clone(),
                });
            }
        };

        self.build_planned_update(request, payload, &allowed_paths, response_id)
    }

    pub fn plan_plugin_edits(
        &self,
        workspace_root: &Path,
        request: PluginPlanRequest,
    ) -> Result<PlannedPluginEdit, RuntimeError> {
        if request.instruction.trim().is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "instruction must not be empty".to_string(),
            });
        }
        if request.context_paths.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "at least one plugin context path is required".to_string(),
            });
        }
        if request.writable_roots.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "at least one writable plugin root is required".to_string(),
            });
        }

        let context_paths = normalize_paths(&request.context_paths)?;
        let writable_roots = normalize_paths(&request.writable_roots)?;
        let context_files = load_context_files(workspace_root, &context_paths)?;
        let planner_context_files = select_plugin_planner_context_files(
            request.instruction.as_str(),
            &context_files,
            &writable_roots,
        );
        let plugin_catalog = discover_plugin_catalog(workspace_root);
        let provider = self.config.provider.trim().to_ascii_lowercase();
        let (payload, response_id) = match provider.as_str() {
            "openai" => self.plan_plugin_edits_with_openai(&build_plugin_edit_prompt(
                &request,
                &planner_context_files,
                plugin_catalog.as_ref(),
            ))?,
            "deepseek" => {
                if model_uses_deepseek_reasoner(&self.config.model) {
                    let tool_prompt = build_deepseek_plugin_tool_prompt(
                        &request,
                        &planner_context_files,
                        plugin_catalog.as_ref(),
                    );
                    let fallback_prompt = build_plugin_edit_prompt(
                        &request,
                        &planner_context_files,
                        plugin_catalog.as_ref(),
                    );
                    if should_try_inline_deepseek_completion(&planner_context_files) {
                        match self.plan_plugin_edits_with_deepseek_completion(&fallback_prompt) {
                            Ok(result) => result,
                            Err(inline_err) if should_fallback_deepseek_reasoner(&inline_err) => {
                                emit_llm_diagnostic(
                                    true,
                                    format!(
                                        "reasoner_fallback operation=plugin_edit_planning mode=tool_loop_retry reason={}",
                                        inline_err
                                    ),
                                );
                                match self.plan_plugin_edits_with_deepseek(
                                    &tool_prompt,
                                    &planner_context_files,
                                    plugin_catalog.as_ref(),
                                ) {
                                    Ok(result) => result,
                                    Err(reasoner_err)
                                        if should_fallback_deepseek_reasoner(&reasoner_err) =>
                                    {
                                        return Err(RuntimeError::LlmRequestFailed {
                                            message: format!(
                                                "DeepSeek inline plugin edit planning failed: {inline_err}; reasoner tool-mode retry failed: {reasoner_err}"
                                            ),
                                        });
                                    }
                                    Err(err) => return Err(err),
                                }
                            }
                            Err(err) => return Err(err),
                        }
                    } else {
                        match self.plan_plugin_edits_with_deepseek(
                            &tool_prompt,
                            &planner_context_files,
                            plugin_catalog.as_ref(),
                        ) {
                            Ok(result) => result,
                            Err(reasoner_err)
                                if should_fallback_deepseek_reasoner(&reasoner_err) =>
                            {
                                emit_llm_diagnostic(
                                    true,
                                    format!(
                                        "reasoner_fallback operation=plugin_edit_planning mode=one_shot_completion reason={}",
                                        reasoner_err
                                    ),
                                );
                                self.plan_plugin_edits_with_deepseek_completion(&fallback_prompt)
                                    .map_err(|fallback_err| {
                                        combine_reasoner_fallback_errors(
                                            "plugin edit planning",
                                            reasoner_err,
                                            fallback_err,
                                        )
                                    })?
                            }
                            Err(err) => return Err(err),
                        }
                    }
                } else {
                    self.plan_plugin_edits_with_deepseek(
                        &build_plugin_edit_prompt(
                            &request,
                            &planner_context_files,
                            plugin_catalog.as_ref(),
                        ),
                        &planner_context_files,
                        plugin_catalog.as_ref(),
                    )?
                }
            }
            _ => {
                return Err(RuntimeError::UnsupportedLlmProvider {
                    provider: self.config.provider.clone(),
                });
            }
        };

        self.build_planned_plugin_edit(request, payload, &writable_roots, response_id)
    }

    fn plan_with_openai(
        &self,
        user_prompt: &str,
    ) -> Result<(PlannerPayload, Option<String>), RuntimeError> {
        let request_body = json!({
            "model": self.config.model,
            "instructions": openai_system_prompt(),
            "input": user_prompt,
            "temperature": self.config.temperature,
            "max_output_tokens": self.config.max_tokens,
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": PLAN_SCHEMA_NAME,
                    "strict": true,
                    "schema": plan_schema(),
                }
            }
        });

        let raw_json = self.send_json_request(
            format!("{}/responses", self.config.base_url.trim_end_matches('/')),
            request_body,
        )?;
        let response_id = raw_json
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let output_text =
            extract_output_text(&raw_json).ok_or_else(|| RuntimeError::LlmResponseInvalid {
                message: "missing output_text in Responses API payload".to_string(),
            })?;
        Ok((parse_planner_payload(&output_text)?, response_id))
    }

    fn plan_plugin_edits_with_openai(
        &self,
        user_prompt: &str,
    ) -> Result<(PluginPlannerPayload, Option<String>), RuntimeError> {
        let request_body = json!({
            "model": self.config.model,
            "instructions": plugin_edit_openai_system_prompt(),
            "input": user_prompt,
            "temperature": self.config.temperature,
            "max_output_tokens": self.config.max_tokens,
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": PLUGIN_EDIT_PLAN_SCHEMA_NAME,
                    "strict": true,
                    "schema": plugin_edit_plan_schema(),
                }
            }
        });

        let raw_json = self.send_json_request(
            format!("{}/responses", self.config.base_url.trim_end_matches('/')),
            request_body,
        )?;
        let response_id = raw_json
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let output_text =
            extract_output_text(&raw_json).ok_or_else(|| RuntimeError::LlmResponseInvalid {
                message: "missing output_text in Responses API payload".to_string(),
            })?;
        Ok((parse_plugin_planner_payload(&output_text)?, response_id))
    }

    fn plan_with_deepseek(
        &self,
        user_prompt: &str,
        context_files: &[ContextFile],
        plugin_catalog: Option<&PromptPluginCatalog>,
    ) -> Result<(PlannerPayload, Option<String>), RuntimeError> {
        if !model_uses_deepseek_reasoner(&self.config.model) {
            return self.plan_with_deepseek_completion(user_prompt);
        }

        let mut inspection_state = DeepSeekReadInspectionState::default();
        self.run_deepseek_tool_loop(
            deepseek_patch_tool_system_prompt(),
            user_prompt,
            deepseek_patch_tools(),
            parse_planner_payload,
            |tool_call| {
                self.execute_deepseek_patch_tool_call(
                    tool_call,
                    context_files,
                    plugin_catalog,
                    &mut inspection_state,
                )
            },
        )
    }

    fn plan_plugin_edits_with_deepseek(
        &self,
        user_prompt: &str,
        context_files: &[ContextFile],
        plugin_catalog: Option<&PromptPluginCatalog>,
    ) -> Result<(PluginPlannerPayload, Option<String>), RuntimeError> {
        if !model_uses_deepseek_reasoner(&self.config.model) {
            return self.plan_plugin_edits_with_deepseek_completion(user_prompt);
        }

        let mut inspection_state = DeepSeekReadInspectionState::default();
        self.run_deepseek_tool_loop(
            deepseek_plugin_tool_system_prompt(),
            user_prompt,
            deepseek_plugin_tools(),
            parse_plugin_planner_payload,
            |tool_call| {
                self.execute_deepseek_plugin_tool_call(
                    tool_call,
                    context_files,
                    plugin_catalog,
                    &mut inspection_state,
                )
            },
        )
    }

    fn plan_with_deepseek_completion(
        &self,
        user_prompt: &str,
    ) -> Result<(PlannerPayload, Option<String>), RuntimeError> {
        self.complete_with_deepseek_json(
            deepseek_system_prompt(),
            user_prompt,
            parse_planner_payload,
        )
    }

    fn plan_plugin_edits_with_deepseek_completion(
        &self,
        user_prompt: &str,
    ) -> Result<(PluginPlannerPayload, Option<String>), RuntimeError> {
        self.complete_with_deepseek_json(
            plugin_edit_deepseek_system_prompt(),
            user_prompt,
            parse_plugin_planner_payload,
        )
    }

    fn complete_with_deepseek_json<T, P>(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        parse_payload: P,
    ) -> Result<(T, Option<String>), RuntimeError>
    where
        P: Fn(&str) -> Result<T, RuntimeError>,
    {
        let completion_model = deepseek_completion_model(&self.config.model);
        let request_body = json!({
            "model": completion_model,
            "messages": [
                {
                    "role": "system",
                    "content": system_prompt,
                },
                {
                    "role": "user",
                    "content": user_prompt,
                }
            ],
            "temperature": self.config.temperature,
            "max_tokens": self.config.max_tokens,
            "response_format": {
                "type": "json_object"
            }
        });

        let (message, response_id, _finish_reason) = self.send_deepseek_chat_request(
            format!(
                "{}/chat/completions",
                self.config.base_url.trim_end_matches('/')
            ),
            request_body,
        )?;
        if !message.tool_calls.is_empty() {
            return Err(RuntimeError::LlmResponseInvalid {
                message: "DeepSeek chat completion returned unexpected tool calls without a tools request".to_string(),
            });
        }
        let output_text = message
            .content
            .ok_or_else(|| RuntimeError::LlmResponseInvalid {
                message: "missing streamed message.content in DeepSeek chat completion payload"
                    .to_string(),
            })?;
        Ok((parse_payload(&output_text)?, response_id))
    }

    fn run_deepseek_tool_loop<T, F, P>(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        tools: Value,
        parse_final_content: P,
        mut handle_tool_call: F,
    ) -> Result<(T, Option<String>), RuntimeError>
    where
        F: FnMut(&DeepSeekToolCall) -> Result<DeepSeekToolOutcome<T>, RuntimeError>,
        P: Fn(&str) -> Result<T, RuntimeError>,
    {
        let endpoint = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut messages = vec![
            json!({
                "role": "system",
                "content": system_prompt,
            }),
            json!({
                "role": "user",
                "content": user_prompt,
            }),
        ];
        let loop_started = Instant::now();
        let total_budget = Duration::from_millis(self.config.timeout_ms);
        let mut turn_idx = 0usize;
        while turn_idx < DEEPSEEK_REASONER_MAX_TURNS_SAFETY {
            let elapsed_before_turn = loop_started.elapsed();
            if elapsed_before_turn >= total_budget {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!(
                        "DeepSeek reasoner exceeded total planning budget after {} turns; elapsed_ms={} timeout_ms={} model={} endpoint={}/chat/completions",
                        turn_idx,
                        elapsed_before_turn.as_millis(),
                        self.config.timeout_ms,
                        self.config.model,
                        self.config.base_url.trim_end_matches('/'),
                    ),
                });
            }
            let turn_started = Instant::now();
            emit_llm_diagnostic(
                false,
                format!(
                    "reasoner_turn_start turn={} elapsed_ms={} budget_ms={} model={} messages={} tool_count={}",
                    turn_idx + 1,
                    elapsed_before_turn.as_millis(),
                    self.config.timeout_ms,
                    self.config.model,
                    messages.len(),
                    tools.as_array().map(|items| items.len()).unwrap_or(0),
                ),
            );
            let request_body = json!({
                "model": self.config.model,
                "messages": messages,
                "temperature": self.config.temperature,
                "max_tokens": self.config.max_tokens,
                "tools": tools,
                "tool_choice": "auto",
            });
            let (message, response_id, finish_reason) =
                self.send_deepseek_chat_request(endpoint.clone(), request_body)?;
            let tool_names = message
                .tool_calls
                .iter()
                .map(|tool_call| tool_call.function.name.as_str())
                .collect::<Vec<_>>();
            emit_llm_diagnostic(
                false,
                format!(
                    "reasoner_turn_result turn={} elapsed_ms={} total_elapsed_ms={} response_id={} tool_calls={} tool_names={:?} reasoning_chars={} content_chars={}",
                    turn_idx + 1,
                    turn_started.elapsed().as_millis(),
                    loop_started.elapsed().as_millis(),
                    response_id.as_deref().unwrap_or("-"),
                    message.tool_calls.len(),
                    tool_names,
                    message
                        .reasoning_content
                        .as_deref()
                        .map(str::len)
                        .unwrap_or(0),
                    message.content.as_deref().map(str::len).unwrap_or(0),
                ),
            );
            if !message.tool_calls.is_empty() {
                messages.push(message.to_request_message());
                for tool_call in &message.tool_calls {
                    match handle_tool_call(tool_call)? {
                        DeepSeekToolOutcome::ToolResult(tool_output) => {
                            emit_llm_diagnostic(
                                false,
                                format!(
                                    "reasoner_tool_feedback turn={} tool={} tool_call_id={} feedback_bytes={}",
                                    turn_idx + 1,
                                    tool_call.function.name,
                                    tool_call.id,
                                    tool_output.len(),
                                ),
                            );
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_call.id,
                                "content": tool_output,
                            }));
                        }
                        DeepSeekToolOutcome::Final(payload) => {
                            emit_llm_diagnostic(
                                false,
                                format!(
                                    "reasoner_turn_submit turn={} tool={} tool_call_id={}",
                                    turn_idx + 1,
                                    tool_call.function.name,
                                    tool_call.id,
                                ),
                            );
                            if message.tool_calls.len() != 1 {
                                return Err(RuntimeError::LlmResponseInvalid {
                                    message: format!(
                                        "terminal planner tool {} must be the only tool call in a DeepSeek reasoner turn",
                                        tool_call.function.name
                                    ),
                                });
                            }
                            return Ok((payload, response_id));
                        }
                    }
                }
                turn_idx += 1;
                continue;
            }

            if let Some(content) = message.content.as_deref() {
                return Ok((parse_final_content(content)?, response_id));
            }

            if matches!(finish_reason.as_deref(), Some("length"))
                && message
                    .reasoning_content
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
            {
                emit_llm_diagnostic(
                    true,
                    format!(
                        "reasoner_turn_continue turn={} response_id={} finish_reason=length action=continue_reasoning reasoning_chars={} total_elapsed_ms={} budget_ms={}",
                        turn_idx + 1,
                        response_id.as_deref().unwrap_or("-"),
                        message
                            .reasoning_content
                            .as_deref()
                            .map(str::len)
                            .unwrap_or(0),
                        loop_started.elapsed().as_millis(),
                        self.config.timeout_ms,
                    ),
                );
                messages.push(message.to_request_message());
                turn_idx += 1;
                continue;
            }

            return Err(RuntimeError::LlmResponseInvalid {
                message: "DeepSeek reasoner response had neither tool_calls nor final content"
                    .to_string(),
            });
        }

        Err(RuntimeError::LlmResponseInvalid {
            message: format!(
                "DeepSeek reasoner exceeded safety turn limit {} without producing a final plan; elapsed_ms={} timeout_ms={} model={} endpoint={}/chat/completions",
                DEEPSEEK_REASONER_MAX_TURNS_SAFETY,
                loop_started.elapsed().as_millis(),
                self.config.timeout_ms,
                self.config.model,
                self.config.base_url.trim_end_matches('/'),
            ),
        })
    }

    fn send_deepseek_chat_request(
        &self,
        endpoint: String,
        mut request_body: Value,
    ) -> Result<(DeepSeekChatMessage, Option<String>, Option<String>), RuntimeError> {
        request_body["stream"] = Value::Bool(true);
        let api_key = resolve_api_key(&self.config)?;
        let request_summary =
            summarize_llm_request(&endpoint, &request_body, self.config.timeout_ms);
        let overall_started = Instant::now();
        emit_llm_diagnostic(
            false,
            format!(
                "request_start attempts={} {}",
                LLM_REQUEST_MAX_ATTEMPTS, request_summary
            ),
        );
        for attempt in 1..=LLM_REQUEST_MAX_ATTEMPTS {
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
                    let detail = format_llm_transport_error(&err, self.config.timeout_ms);
                    let message = format!(
                        "llm request failed: attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} phase=send elapsed_ms={} total_elapsed_ms={} {} detail={detail}",
                        attempt_started.elapsed().as_millis(),
                        overall_started.elapsed().as_millis(),
                        request_summary,
                    );
                    if attempt < LLM_REQUEST_MAX_ATTEMPTS {
                        emit_llm_diagnostic(
                            true,
                            format!("{message} retry_backoff_ms={LLM_REQUEST_RETRY_BACKOFF_MS}"),
                        );
                        thread::sleep(Duration::from_millis(LLM_REQUEST_RETRY_BACKOFF_MS));
                        continue;
                    }
                    emit_llm_diagnostic(true, message.clone());
                    return Err(RuntimeError::LlmRequestFailed { message });
                }
            };

            let status = response.status();
            if !status.is_success() {
                let raw_body = match response.text() {
                    Ok(body) => body,
                    Err(err) => {
                        let detail = format_llm_transport_error(&err, self.config.timeout_ms);
                        let message = format!(
                            "llm request failed: attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} phase=read_error_body elapsed_ms={} total_elapsed_ms={} {} detail={detail}",
                            attempt_started.elapsed().as_millis(),
                            overall_started.elapsed().as_millis(),
                            request_summary,
                        );
                        if attempt < LLM_REQUEST_MAX_ATTEMPTS {
                            emit_llm_diagnostic(
                                true,
                                format!(
                                    "{message} retry_backoff_ms={LLM_REQUEST_RETRY_BACKOFF_MS}"
                                ),
                            );
                            thread::sleep(Duration::from_millis(LLM_REQUEST_RETRY_BACKOFF_MS));
                            continue;
                        }
                        emit_llm_diagnostic(true, message.clone());
                        return Err(RuntimeError::LlmRequestFailed { message });
                    }
                };
                let message = format!(
                    "llm request failed: attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} phase=http_status status={} elapsed_ms={} total_elapsed_ms={} {} error={} body_preview={}",
                    status.as_u16(),
                    attempt_started.elapsed().as_millis(),
                    overall_started.elapsed().as_millis(),
                    request_summary,
                    extract_error_message(&raw_body)
                        .unwrap_or_else(|| format!("status={} body={}", status, raw_body.trim())),
                    truncate_for_error(&raw_body, 400),
                );
                if attempt < LLM_REQUEST_MAX_ATTEMPTS
                    && (status.is_server_error() || status.as_u16() == 429)
                {
                    emit_llm_diagnostic(
                        true,
                        format!("{message} retry_backoff_ms={LLM_REQUEST_RETRY_BACKOFF_MS}"),
                    );
                    thread::sleep(Duration::from_millis(LLM_REQUEST_RETRY_BACKOFF_MS));
                    continue;
                }
                emit_llm_diagnostic(true, message.clone());
                return Err(RuntimeError::LlmRequestFailed { message });
            }

            let streamed = match self.read_deepseek_chat_stream(response, &request_summary, attempt)
            {
                Ok(streamed) => streamed,
                Err(DeepSeekStreamReadError::Io(err)) => {
                    let detail = format_llm_stream_io_error(&err, self.config.timeout_ms);
                    let message = format!(
                        "llm request failed: attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} phase=read_stream elapsed_ms={} total_elapsed_ms={} {} detail={detail}",
                        attempt_started.elapsed().as_millis(),
                        overall_started.elapsed().as_millis(),
                        request_summary,
                    );
                    if attempt < LLM_REQUEST_MAX_ATTEMPTS {
                        emit_llm_diagnostic(
                            true,
                            format!("{message} retry_backoff_ms={LLM_REQUEST_RETRY_BACKOFF_MS}"),
                        );
                        thread::sleep(Duration::from_millis(LLM_REQUEST_RETRY_BACKOFF_MS));
                        continue;
                    }
                    emit_llm_diagnostic(true, message.clone());
                    return Err(RuntimeError::LlmRequestFailed { message });
                }
                Err(DeepSeekStreamReadError::InvalidResponse(message)) => {
                    emit_llm_diagnostic(true, message.clone());
                    return Err(RuntimeError::LlmResponseInvalid { message });
                }
            };

            emit_llm_diagnostic(
                false,
                format!(
                    "request_success attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} status={} elapsed_ms={} total_elapsed_ms={} response_bytes={} stream_events={} stream_done={} {}",
                    status.as_u16(),
                    attempt_started.elapsed().as_millis(),
                    overall_started.elapsed().as_millis(),
                    streamed.raw_bytes,
                    streamed.event_count,
                    streamed.saw_done,
                    request_summary,
                ),
            );
            return Ok((
                streamed.message,
                streamed.response_id,
                streamed.finish_reason,
            ));
        }

        let message = format!(
            "llm request exhausted retries without returning a streamed response: total_elapsed_ms={} {}",
            overall_started.elapsed().as_millis(),
            request_summary,
        );
        emit_llm_diagnostic(true, message.clone());
        Err(RuntimeError::LlmRequestFailed { message })
    }

    fn read_deepseek_chat_stream(
        &self,
        response: Response,
        request_summary: &str,
        attempt: usize,
    ) -> Result<DeepSeekChatStreamReadResult, DeepSeekStreamReadError> {
        let mut reader = BufReader::new(response);
        let mut raw_bytes = 0usize;
        let mut event_count = 0usize;
        let mut saw_done = false;
        let mut finish_reason = None;
        let mut pending_data_lines = Vec::new();
        let mut accumulator = DeepSeekChatMessageAccumulator::default();

        loop {
            let mut line = String::new();
            let bytes_read = reader
                .read_line(&mut line)
                .map_err(DeepSeekStreamReadError::Io)?;
            raw_bytes += bytes_read;
            if bytes_read == 0 {
                if !pending_data_lines.is_empty() {
                    saw_done = process_deepseek_stream_event(
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
                if process_deepseek_stream_event(
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
            .map_err(|err| DeepSeekStreamReadError::InvalidResponse(err.to_string()))?;
        Ok(DeepSeekChatStreamReadResult {
            response_id,
            message,
            raw_bytes,
            event_count,
            saw_done,
            finish_reason,
        })
    }

    fn execute_deepseek_patch_tool_call(
        &self,
        tool_call: &DeepSeekToolCall,
        context_files: &[ContextFile],
        plugin_catalog: Option<&PromptPluginCatalog>,
        inspection_state: &mut DeepSeekReadInspectionState,
    ) -> Result<DeepSeekToolOutcome<PlannerPayload>, RuntimeError> {
        match tool_call.function.name.as_str() {
            DEEPSEEK_TOOL_LIST_CONTEXT_FILES => {
                parse_tool_arguments::<Value>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                Ok(DeepSeekToolOutcome::ToolResult(
                    json!({
                        "paths": context_files
                            .iter()
                            .map(|file| json!({
                                "path": file.path,
                                "sha256": file.sha256,
                            }))
                            .collect::<Vec<_>>(),
                    })
                    .to_string(),
                ))
            }
            DEEPSEEK_TOOL_READ_CONTEXT_FILES => {
                let args = parse_tool_arguments::<ReadContextFilesArgs>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                let files = find_context_files(context_files, &args.paths)?;
                let observation =
                    inspection_state.record_read(files.iter().map(|file| file.path.as_str()));
                emit_read_observation_diagnostic(
                    DeepSeekPlannerMode::Patch,
                    &tool_call.function.name,
                    &observation,
                );
                Ok(DeepSeekToolOutcome::ToolResult(build_read_tool_result(
                    DeepSeekPlannerMode::Patch,
                    json!({
                        "files": files
                            .into_iter()
                            .map(|file| json!({
                                "path": file.path,
                                "sha256": file.sha256,
                                "content": file.content,
                            }))
                            .collect::<Vec<_>>(),
                    }),
                    &observation,
                )))
            }
            DEEPSEEK_TOOL_READ_CONTEXT_FILE => {
                let args = parse_tool_arguments::<ReadContextFileArgs>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                let file = find_context_file(context_files, &args.path)?;
                let observation = inspection_state.record_read([file.path.as_str()]);
                emit_read_observation_diagnostic(
                    DeepSeekPlannerMode::Patch,
                    &tool_call.function.name,
                    &observation,
                );
                Ok(DeepSeekToolOutcome::ToolResult(build_read_tool_result(
                    DeepSeekPlannerMode::Patch,
                    json!({
                        "path": file.path,
                        "sha256": file.sha256,
                        "content": file.content,
                    }),
                    &observation,
                )))
            }
            DEEPSEEK_TOOL_INSPECT_PLUGIN_CATALOG => {
                let args = parse_tool_arguments::<InspectPluginCatalogArgs>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                Ok(DeepSeekToolOutcome::ToolResult(
                    render_plugin_catalog_tool_result(plugin_catalog, args.plugin_path.as_deref())?
                        .to_string(),
                ))
            }
            DEEPSEEK_TOOL_SUBMIT_PATCH_PLAN => {
                let payload = parse_tool_arguments::<PlannerPayload>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                let allowed_paths = context_files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect::<BTreeSet<_>>();
                match validate_submitted_patch_payload(&payload, &allowed_paths) {
                    Ok(()) => Ok(DeepSeekToolOutcome::Final(payload)),
                    Err(err) => Ok(DeepSeekToolOutcome::ToolResult(
                        tool_feedback_error(&tool_call.function.name, &err.to_string()).to_string(),
                    )),
                }
            }
            other => Err(RuntimeError::LlmResponseInvalid {
                message: format!("unknown DeepSeek planner tool call: {other}"),
            }),
        }
    }

    fn execute_deepseek_plugin_tool_call(
        &self,
        tool_call: &DeepSeekToolCall,
        context_files: &[ContextFile],
        plugin_catalog: Option<&PromptPluginCatalog>,
        inspection_state: &mut DeepSeekReadInspectionState,
    ) -> Result<DeepSeekToolOutcome<PluginPlannerPayload>, RuntimeError> {
        let writable_roots = context_files
            .iter()
            .filter_map(|file| {
                file.path
                    .strip_suffix("/Cargo.toml")
                    .map(ToString::to_string)
            })
            .collect::<BTreeSet<_>>();
        match tool_call.function.name.as_str() {
            DEEPSEEK_TOOL_LIST_CONTEXT_FILES => {
                parse_tool_arguments::<Value>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                Ok(DeepSeekToolOutcome::ToolResult(
                    json!({
                        "paths": context_files
                            .iter()
                            .map(|file| {
                                let writable = path_within_writable_surface(&file.path, &writable_roots);
                                json!({
                                    "path": file.path,
                                    "sha256": file.sha256,
                                    "writable": writable,
                                    "generated_read_only": !writable,
                                })
                            })
                            .collect::<Vec<_>>(),
                    })
                    .to_string(),
                ))
            }
            DEEPSEEK_TOOL_READ_CONTEXT_FILES => {
                let args = parse_tool_arguments::<ReadContextFilesArgs>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                let files = find_context_files(context_files, &args.paths)?;
                let observation =
                    inspection_state.record_read(files.iter().map(|file| file.path.as_str()));
                emit_read_observation_diagnostic(
                    DeepSeekPlannerMode::Plugin,
                    &tool_call.function.name,
                    &observation,
                );
                Ok(DeepSeekToolOutcome::ToolResult(build_read_tool_result(
                    DeepSeekPlannerMode::Plugin,
                    json!({
                        "files": files
                            .into_iter()
                            .map(|file| {
                                let writable =
                                    path_within_writable_surface(&file.path, &writable_roots);
                                json!({
                                    "path": file.path,
                                    "sha256": file.sha256,
                                    "writable": writable,
                                    "generated_read_only": !writable,
                                    "content": file.content,
                                })
                            })
                            .collect::<Vec<_>>(),
                    }),
                    &observation,
                )))
            }
            DEEPSEEK_TOOL_READ_CONTEXT_FILE => {
                let args = parse_tool_arguments::<ReadContextFileArgs>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                let file = find_context_file(context_files, &args.path)?;
                let writable = path_within_writable_surface(&file.path, &writable_roots);
                let observation = inspection_state.record_read([file.path.as_str()]);
                emit_read_observation_diagnostic(
                    DeepSeekPlannerMode::Plugin,
                    &tool_call.function.name,
                    &observation,
                );
                Ok(DeepSeekToolOutcome::ToolResult(build_read_tool_result(
                    DeepSeekPlannerMode::Plugin,
                    json!({
                        "path": file.path,
                        "sha256": file.sha256,
                        "writable": writable,
                        "generated_read_only": !writable,
                        "content": file.content,
                    }),
                    &observation,
                )))
            }
            DEEPSEEK_TOOL_INSPECT_PLUGIN_CATALOG => {
                let args = parse_tool_arguments::<InspectPluginCatalogArgs>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                Ok(DeepSeekToolOutcome::ToolResult(
                    render_plugin_catalog_tool_result(plugin_catalog, args.plugin_path.as_deref())?
                        .to_string(),
                ))
            }
            DEEPSEEK_TOOL_SUBMIT_PLUGIN_EDIT_PLAN => {
                let payload = parse_tool_arguments::<PluginPlannerPayload>(
                    &tool_call.function.arguments,
                    &tool_call.function.name,
                )?;
                match validate_submitted_plugin_payload(&payload, &writable_roots) {
                    Ok(()) => Ok(DeepSeekToolOutcome::Final(payload)),
                    Err(err) => Ok(DeepSeekToolOutcome::ToolResult(
                        tool_feedback_error(&tool_call.function.name, &err.to_string()).to_string(),
                    )),
                }
            }
            other => Err(RuntimeError::LlmResponseInvalid {
                message: format!("unknown DeepSeek plugin planner tool call: {other}"),
            }),
        }
    }

    fn send_json_request(
        &self,
        endpoint: String,
        request_body: Value,
    ) -> Result<Value, RuntimeError> {
        let api_key = resolve_api_key(&self.config)?;
        let request_summary =
            summarize_llm_request(&endpoint, &request_body, self.config.timeout_ms);
        let overall_started = Instant::now();
        emit_llm_diagnostic(
            false,
            format!(
                "request_start attempts={} {}",
                LLM_REQUEST_MAX_ATTEMPTS, request_summary
            ),
        );
        for attempt in 1..=LLM_REQUEST_MAX_ATTEMPTS {
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
                    let detail = format_llm_transport_error(&err, self.config.timeout_ms);
                    let message = format!(
                        "llm request failed: attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} phase=send elapsed_ms={} total_elapsed_ms={} {} detail={detail}",
                        attempt_started.elapsed().as_millis(),
                        overall_started.elapsed().as_millis(),
                        request_summary,
                    );
                    if attempt < LLM_REQUEST_MAX_ATTEMPTS {
                        emit_llm_diagnostic(
                            true,
                            format!("{message} retry_backoff_ms={LLM_REQUEST_RETRY_BACKOFF_MS}"),
                        );
                        thread::sleep(Duration::from_millis(LLM_REQUEST_RETRY_BACKOFF_MS));
                        continue;
                    }
                    emit_llm_diagnostic(true, message.clone());
                    return Err(RuntimeError::LlmRequestFailed { message });
                }
            };

            let status = response.status();
            let raw_body = match response.text() {
                Ok(body) => body,
                Err(err) => {
                    let detail = format_llm_transport_error(&err, self.config.timeout_ms);
                    let message = format!(
                        "llm request failed: attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} phase=read_body elapsed_ms={} total_elapsed_ms={} {} detail={detail}",
                        attempt_started.elapsed().as_millis(),
                        overall_started.elapsed().as_millis(),
                        request_summary,
                    );
                    if attempt < LLM_REQUEST_MAX_ATTEMPTS {
                        emit_llm_diagnostic(
                            true,
                            format!("{message} retry_backoff_ms={LLM_REQUEST_RETRY_BACKOFF_MS}"),
                        );
                        thread::sleep(Duration::from_millis(LLM_REQUEST_RETRY_BACKOFF_MS));
                        continue;
                    }
                    emit_llm_diagnostic(true, message.clone());
                    return Err(RuntimeError::LlmRequestFailed { message });
                }
            };

            if !status.is_success() {
                let message = format!(
                    "llm request failed: attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} phase=http_status status={} elapsed_ms={} total_elapsed_ms={} {} error={} body_preview={}",
                    status.as_u16(),
                    attempt_started.elapsed().as_millis(),
                    overall_started.elapsed().as_millis(),
                    request_summary,
                    extract_error_message(&raw_body)
                        .unwrap_or_else(|| format!("status={} body={}", status, raw_body.trim())),
                    truncate_for_error(&raw_body, 400),
                );
                if attempt < LLM_REQUEST_MAX_ATTEMPTS
                    && (status.is_server_error() || status.as_u16() == 429)
                {
                    emit_llm_diagnostic(
                        true,
                        format!("{message} retry_backoff_ms={LLM_REQUEST_RETRY_BACKOFF_MS}"),
                    );
                    thread::sleep(Duration::from_millis(LLM_REQUEST_RETRY_BACKOFF_MS));
                    continue;
                }
                emit_llm_diagnostic(true, message.clone());
                return Err(RuntimeError::LlmRequestFailed { message });
            }

            let parsed = match serde_json::from_str(&raw_body) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let message = format!(
                        "invalid JSON response: attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} phase=parse_json elapsed_ms={} total_elapsed_ms={} {} parse_error={err}; body_preview={}",
                        attempt_started.elapsed().as_millis(),
                        overall_started.elapsed().as_millis(),
                        request_summary,
                        truncate_for_error(&raw_body, 800)
                    );
                    emit_llm_diagnostic(true, message.clone());
                    return Err(RuntimeError::LlmResponseInvalid { message });
                }
            };
            emit_llm_diagnostic(
                false,
                format!(
                    "request_success attempt={attempt}/{LLM_REQUEST_MAX_ATTEMPTS} status={} elapsed_ms={} total_elapsed_ms={} response_bytes={} {}",
                    status.as_u16(),
                    attempt_started.elapsed().as_millis(),
                    overall_started.elapsed().as_millis(),
                    raw_body.len(),
                    request_summary,
                ),
            );
            return Ok(parsed);
        }

        let message = format!(
            "llm request exhausted retries without returning a response: total_elapsed_ms={} {}",
            overall_started.elapsed().as_millis(),
            request_summary,
        );
        emit_llm_diagnostic(true, message.clone());
        Err(RuntimeError::LlmRequestFailed { message })
    }

    fn build_planned_update(
        &self,
        request: PlanRequest,
        payload: PlannerPayload,
        allowed_paths: &BTreeSet<String>,
        response_id: Option<String>,
    ) -> Result<PlannedUpdate, RuntimeError> {
        if payload.patches.is_empty() {
            return Err(RuntimeError::LlmResponseInvalid {
                message: "model returned zero patches".to_string(),
            });
        }
        validate_patch_paths(&payload.patches, allowed_paths)?;

        let diff_lines = estimate_diff_lines(&payload.patches);
        Ok(PlannedUpdate {
            plan: AutoUpdatePlan {
                issue_id: request.issue_id,
                patch_id: request.patch_id,
                manual_approved: request.manual_approved,
                diff_lines,
                patches: payload.patches,
            },
            summary: payload.summary,
            tests_command: normalize_optional_command(payload.tests_command),
            safety_command: normalize_optional_command(payload.safety_command),
            planner_model: self.config.model.clone(),
            response_id,
        })
    }

    fn build_planned_plugin_edit(
        &self,
        request: PluginPlanRequest,
        payload: PluginPlannerPayload,
        writable_roots: &BTreeSet<String>,
        response_id: Option<String>,
    ) -> Result<PlannedPluginEdit, RuntimeError> {
        if payload.operations.is_empty() {
            return Err(RuntimeError::LlmResponseInvalid {
                message: "model returned zero plugin edit operations".to_string(),
            });
        }
        validate_plugin_operation_paths(&payload.operations, writable_roots)?;

        Ok(PlannedPluginEdit {
            plan: PluginEditPlan {
                issue_id: request.issue_id,
                patch_id: request.patch_id,
                summary: payload.summary.clone(),
                operations: payload.operations,
            },
            summary: payload.summary,
            tests_command: normalize_optional_command(payload.tests_command),
            safety_command: normalize_optional_command(payload.safety_command),
            planner_model: self.config.model.clone(),
            response_id,
        })
    }
}

fn normalize_paths(paths: &[String]) -> Result<BTreeSet<String>, RuntimeError> {
    let mut normalized = BTreeSet::new();
    for path in paths {
        let safe_path = normalize_rel_path(path)?;
        normalized.insert(safe_path);
    }
    Ok(normalized)
}

fn load_context_files(
    workspace_root: &Path,
    paths: &BTreeSet<String>,
) -> Result<Vec<ContextFile>, RuntimeError> {
    let mut files = Vec::with_capacity(paths.len());
    for rel_path in paths {
        let abs_path = workspace_root.join(rel_path);
        let content = fs::read_to_string(&abs_path).map_err(|err| RuntimeError::Io {
            path: abs_path.clone(),
            message: err.to_string(),
        })?;
        files.push(ContextFile {
            path: rel_path.clone(),
            sha256: sha256_text(&content),
            content,
        });
    }
    Ok(files)
}

fn build_user_prompt(
    request: &PlanRequest,
    files: &[ContextFile],
    plugin_catalog: Option<&PromptPluginCatalog>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Issue ID: ");
    prompt.push_str(&request.issue_id);
    prompt.push_str("\nPatch ID: ");
    prompt.push_str(&request.patch_id);
    prompt.push_str("\nManual approval: ");
    prompt.push_str(if request.manual_approved {
        "true"
    } else {
        "false"
    });
    prompt.push_str("\nInstruction:\n");
    prompt.push_str(request.instruction.trim());
    prompt.push_str("\n\nAllowed relative paths:\n");
    for path in &request.paths {
        prompt.push_str("- ");
        prompt.push_str(path);
        prompt.push('\n');
    }
    prompt.push_str("\nGeneric verifier command formats:\n");
    prompt.push_str("- shell command string executed in the workspace root\n");
    prompt.push_str("- plugin verifier spec: plugin:{\"plugin_path\":\"<plugin_path>\",\"node_id\":\"<node_id>\",\"payload_json\":{},\"expect_substring\":\"<expected text>\",\"fixtures_root\":\"<optional fixtures root>\"}\n");
    prompt.push_str("Prefer plugin verifier specs over shell commands when a discovered plugin can validate the requested behavior directly.\n");
    prompt.push_str("Use only plugin_path/node_id pairs that appear in the discovered plugin catalog below. `command_name` is an optional human-facing alias and not required in plugin verifier specs.\n");
    prompt.push_str("\nSupported patch kinds:\n");
    prompt.push_str("- text: use `find` + `replace` for source edits\n");
    prompt.push_str("- json_value: use `pointer` + `value` for JSON config edits\n");
    prompt.push_str("- toml_value: use `dotted_key` + `value` for TOML config edits\n");
    prompt.push_str("\nDiscovered plugin capability catalog:\n");
    match plugin_catalog {
        Some(catalog) => prompt.push_str(&render_plugin_catalog(catalog)),
        None => prompt.push_str("(none discovered for this workspace root)\n"),
    }
    prompt.push_str("\nCurrent file contents:\n");
    for file in files {
        prompt.push_str("\n<<<FILE ");
        prompt.push_str(&file.path);
        prompt.push_str(">>>\n");
        prompt.push_str(&file.content);
        if !file.content.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("<<<END FILE>>>\n");
    }
    prompt
}

fn build_plugin_edit_prompt(
    request: &PluginPlanRequest,
    files: &[ContextFile],
    plugin_catalog: Option<&PromptPluginCatalog>,
) -> String {
    let writable_roots = request
        .writable_roots
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut prompt = String::new();
    prompt.push_str("Issue ID: ");
    prompt.push_str(&request.issue_id);
    prompt.push_str("\nPatch ID: ");
    prompt.push_str(&request.patch_id);
    prompt.push_str("\nManual approval: ");
    prompt.push_str(if request.manual_approved {
        "true"
    } else {
        "false"
    });
    prompt.push_str("\nInstruction:\n");
    prompt.push_str(request.instruction.trim());
    prompt.push_str("\n\nWritable plugin roots:\n");
    for root in &request.writable_roots {
        prompt.push_str("- ");
        prompt.push_str(root);
        prompt.push('\n');
        prompt.push_str("  surface: ");
        prompt.push_str(root);
        prompt.push_str("/Cargo.toml, ");
        prompt.push_str(root);
        prompt.push_str("/src/**, ");
        prompt.push_str(root);
        prompt.push_str("/tests/**, ");
        prompt.push_str(root);
        prompt.push_str("/docs/human/**\n");
        prompt.push_str("  read-only generated context: ");
        prompt.push_str(root);
        prompt.push_str("/docs/agent/**\n");
    }
    prompt.push_str("\nPlugin edit operation rules:\n");
    prompt.push_str("- `replace_exact`: requires `path`, `expected_old_string`, `new_content`\n");
    prompt.push_str(
        "- `create_file`: requires `path`, `expected_old_string` set to \"\", `new_content`\n",
    );
    prompt.push_str("- `delete_file`: requires `path`, `expected_sha256`\n");
    prompt.push_str("- `json_set`: requires `path`, `expected_sha256`, `pointer`, `value`\n");
    prompt.push_str("- `toml_set`: requires `path`, `expected_sha256`, `dotted_key`, `value`\n");
    prompt.push_str("Every operation must stay inside the writable plugin roots and include the required precondition fields.\n");
    prompt.push_str("Files under `docs/agent/**` may appear below as read-only generated context to help you choose the real source edit, but you must not emit operations that modify them.\n");
    prompt.push_str("\nGeneric verifier command formats:\n");
    prompt.push_str("- shell command string executed in the workspace root\n");
    prompt.push_str("- plugin verifier spec: plugin:{\"plugin_path\":\"<plugin_path>\",\"node_id\":\"<node_id>\",\"payload_json\":{},\"expect_substring\":\"<expected text>\",\"fixtures_root\":\"<optional fixtures root>\"}\n");
    prompt.push_str("Prefer plugin verifier specs over shell commands when a discovered plugin can validate the requested behavior directly.\n");
    prompt.push_str("\nDiscovered plugin capability catalog:\n");
    match filter_plugin_catalog(plugin_catalog, &writable_roots) {
        Some(catalog) => prompt.push_str(&render_plugin_catalog(&catalog)),
        None => prompt.push_str("(none discovered for this workspace root)\n"),
    }
    let read_only_paths = files
        .iter()
        .filter(|file| !path_within_writable_surface(&file.path, &writable_roots))
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    if !read_only_paths.is_empty() {
        prompt.push_str("\nRead-only context files:\n");
        for path in read_only_paths {
            prompt.push_str("- ");
            prompt.push_str(path);
            prompt.push('\n');
        }
    }
    if files
        .iter()
        .any(|file| is_plugin_architecture_guidance_path(&file.path, &writable_roots))
    {
        prompt.push_str("\nArchitecture guidance:\n");
        prompt.push_str("- Human docs under docs/human/overview.md describe intended plugin topology and preferred extension patterns.\n");
        prompt.push_str("- When these docs explain how new behavior should be added, prefer following that architecture over choosing the smallest immediate diff.\n");
    }
    prompt.push_str("\nCurrent file contents with sha256 preconditions:\n");
    for file in files {
        let writable = path_within_writable_surface(&file.path, &writable_roots);
        prompt.push_str("\n<<<FILE ");
        prompt.push_str(&file.path);
        prompt.push_str(" sha256=");
        prompt.push_str(&file.sha256);
        prompt.push_str(">>>\n");
        if writable {
            prompt.push_str(&file.content);
            if !file.content.ends_with('\n') {
                prompt.push('\n');
            }
        } else {
            prompt.push_str(
                "Read-only generated context omitted from the inline prompt to conserve tokens. Use tool mode if you need this file's exact content.\n",
            );
        }
        prompt.push_str("<<<END FILE>>>\n");
    }
    prompt
}

fn build_deepseek_patch_tool_prompt(
    request: &PlanRequest,
    files: &[ContextFile],
    plugin_catalog: Option<&PromptPluginCatalog>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Issue ID: ");
    prompt.push_str(&request.issue_id);
    prompt.push_str("\nPatch ID: ");
    prompt.push_str(&request.patch_id);
    prompt.push_str("\nManual approval: ");
    prompt.push_str(if request.manual_approved {
        "true"
    } else {
        "false"
    });
    prompt.push_str("\nInstruction:\n");
    prompt.push_str(request.instruction.trim());
    prompt.push_str("\n\nUse the available tools to inspect current workspace state before submitting a plan.\n");
    prompt.push_str(
        "Prefer `read_context_files` to inspect several related files in one turn when the task spans multiple sources.\n",
    );
    prompt.push_str(
        "If a read tool reports that the same files were already inspected, stop looping on context gathering: decide the change strategy and either submit the plan or inspect only one missing file that would materially change it.\n",
    );
    prompt.push_str(
        "When you are ready, call `submit_patch_plan` as the only tool call in that message.\n",
    );
    prompt.push_str("If a tool responds with ok=false, revise your plan and submit again.\n");
    prompt.push_str("Do not emit markdown or prose summaries outside tool calls.\n");
    prompt.push_str("\nAvailable context files:\n");
    for file in files {
        prompt.push_str("- ");
        prompt.push_str(&file.path);
        prompt.push_str(" sha256=");
        prompt.push_str(&file.sha256);
        prompt.push('\n');
    }
    prompt.push_str("\nPlugin catalog: ");
    prompt.push_str(if plugin_catalog.is_some() {
        "available via inspect_plugin_catalog"
    } else {
        "not available for this workspace"
    });
    prompt.push('\n');
    prompt
}

fn build_deepseek_plugin_tool_prompt(
    request: &PluginPlanRequest,
    files: &[ContextFile],
    plugin_catalog: Option<&PromptPluginCatalog>,
) -> String {
    let writable_roots = request
        .writable_roots
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut prompt = String::new();
    prompt.push_str("Issue ID: ");
    prompt.push_str(&request.issue_id);
    prompt.push_str("\nPatch ID: ");
    prompt.push_str(&request.patch_id);
    prompt.push_str("\nManual approval: ");
    prompt.push_str(if request.manual_approved {
        "true"
    } else {
        "false"
    });
    prompt.push_str("\nInstruction:\n");
    prompt.push_str(request.instruction.trim());
    prompt.push_str(
        "\n\nUse the available tools to inspect current plugin context before submitting a plan.\n",
    );
    prompt.push_str(
        "Prefer `read_context_files` to inspect the most relevant writable source and test files together instead of reading them one by one.\n",
    );
    prompt.push_str("If a read tool reports that the same files were already inspected, stop looping on context gathering: decide the change strategy (for example new_child_plugin, inline_core_patch, metadata_only, or docs_only) and either submit the plan or inspect only one missing file that would materially change it.\n");
    prompt.push_str("When you are ready, call `submit_plugin_edit_plan` as the only tool call in that message.\n");
    prompt.push_str("If a tool responds with ok=false, revise your plan and submit again.\n");
    prompt.push_str("Files under docs/agent/** are read-only generated context and must never appear in submitted operations.\n");
    prompt.push_str("Human docs under docs/human/overview.md describe intended plugin topology and preferred extension patterns; treat them as architecture guidance, not just prose.\n");
    prompt.push_str("\nWritable plugin roots:\n");
    for root in &request.writable_roots {
        prompt.push_str("- ");
        prompt.push_str(root);
        prompt.push_str(
            " (writable: Cargo.toml, src/**, tests/**, docs/human/**; read-only: docs/agent/**)\n",
        );
    }
    prompt.push_str("\nAvailable context files:\n");
    for file in files {
        let writable = path_within_writable_surface(&file.path, &writable_roots);
        prompt.push_str("- ");
        prompt.push_str(&file.path);
        prompt.push_str(" sha256=");
        prompt.push_str(&file.sha256);
        prompt.push_str(if writable {
            " writable"
        } else {
            " read_only_generated"
        });
        prompt.push('\n');
    }
    prompt.push_str("\nPlugin catalog: ");
    prompt.push_str(if plugin_catalog.is_some() {
        "available via inspect_plugin_catalog"
    } else {
        "not available for this workspace"
    });
    prompt.push('\n');
    prompt
}

fn discover_plugin_catalog(workspace_root: &Path) -> Option<PromptPluginCatalog> {
    let discovery_root = resolve_plugin_catalog_root(workspace_root)?;
    let invoker = PluginInvoker::load(&discovery_root).ok()?;
    let plugins = invoker
        .plugin_registry()
        .iter()
        .filter_map(|(_, plugin)| plugin.docs.as_ref().map(prompt_plugin_info))
        .collect::<Vec<_>>();
    if plugins.is_empty() {
        return None;
    }

    Some(PromptPluginCatalog {
        discovery_root: discovery_root.display().to_string(),
        plugins,
    })
}

fn resolve_plugin_catalog_root(workspace_root: &Path) -> Option<PathBuf> {
    if workspace_root.join("plugins").exists() {
        return Some(workspace_root.to_path_buf());
    }

    let nested_fixtures = workspace_root.join("fixtures");
    if nested_fixtures.join("plugins").exists() {
        return Some(nested_fixtures);
    }

    None
}

fn prompt_plugin_info(docs: &PluginDocs) -> PromptPluginInfo {
    PromptPluginInfo {
        plugin_path: docs.plugin_path.clone(),
        plugin_id: docs.plugin_id.clone(),
        plugin_version: docs.plugin_version.clone(),
        command_name: docs.command_name.clone(),
        nodes: docs
            .nodes
            .iter()
            .map(|node| PromptPluginNodeInfo {
                node_id: node.id.clone(),
                node_fqn: format!("{}::{}", docs.plugin_path, node.id),
                summary: node.summary.clone(),
                input_schema: node.input_schema.clone(),
                output_schema: node.output_schema.clone(),
                side_effects: node.side_effects.clone(),
                failure_modes: node.failure_modes.clone(),
            })
            .collect(),
    }
}

fn render_plugin_catalog(catalog: &PromptPluginCatalog) -> String {
    let catalog_json = serde_json::to_string_pretty(catalog).unwrap_or_else(|_| "{}".to_string());
    let mut rendered = String::new();
    rendered.push_str(&catalog_json);
    rendered.push('\n');
    rendered
}

fn filter_plugin_catalog(
    plugin_catalog: Option<&PromptPluginCatalog>,
    writable_roots: &BTreeSet<String>,
) -> Option<PromptPluginCatalog> {
    let catalog = plugin_catalog?;
    let roots = writable_roots
        .iter()
        .map(|root| root.trim_start_matches("plugins/"))
        .collect::<Vec<_>>();
    let plugins = catalog
        .plugins
        .iter()
        .filter(|plugin| {
            roots.iter().any(|root| {
                plugin.plugin_path == *root || plugin.plugin_path.starts_with(&format!("{root}/"))
            })
        })
        .cloned()
        .collect::<Vec<_>>();

    if plugins.is_empty() {
        None
    } else {
        Some(PromptPluginCatalog {
            discovery_root: catalog.discovery_root.clone(),
            plugins,
        })
    }
}

fn openai_system_prompt() -> &'static str {
    "You are a guarded patch planner for a Rust workspace. Return only JSON that matches the provided schema. \
Only edit the allowed relative paths. Each patch must use an exact substring from the current file contents in `find`. \
Keep changes minimal, deterministic, and safe. Prefer `json_value`/`toml_value` patches for JSON or TOML config files and prefer plugin verifier specs over shell commands when the plugin catalog is sufficient. \
Do not invent files, do not use absolute paths, do not invent plugin capabilities beyond the provided catalog, and do not include markdown. \
When you can infer reliable verification steps from the request and plugin catalog, include `tests_command` and/or `safety_command`; otherwise leave them null or omit them."
}

fn plugin_edit_openai_system_prompt() -> &'static str {
    "You are a guarded plugin-edit planner for a Rust plugin workspace. Return only JSON that matches the provided schema. \
Only emit operations inside the writable plugin roots. Do not modify runtime crates, root manifests, config, .git, target, artifacts, or generated agent docs under docs/agent/**. \
Prefer replace_exact/json_set/toml_set over broad rewrites, and only use create_file/delete_file when necessary. \
For existing files, use the provided sha256 values as expected preconditions. For create_file, set expected_old_string to an empty string. \
Do not invent plugin capabilities beyond the provided catalog and do not include markdown."
}

fn deepseek_system_prompt() -> &'static str {
    "You are a guarded patch planner for a Rust workspace. Return exactly one JSON object and no markdown. \
The JSON must use this shape: {\"summary\":\"short summary\",\"tests_command\":\"optional verifier command or null\",\"safety_command\":\"optional verifier command or null\",\"patches\":[{\"path\":\"relative/path.txt\",\"kind\":\"text|json_value|toml_value\",\"find\":\"exact old text when kind=text\",\"replace\":\"new text when kind=text\",\"pointer\":\"json pointer when kind=json_value\",\"dotted_key\":\"toml dotted key when kind=toml_value\",\"value\":\"replacement value when kind is structured\"}]}. \
Only edit the allowed relative paths. Each text patch must use an exact substring from the current file contents in `find`. \
Keep changes minimal, deterministic, and safe. Prefer `json_value`/`toml_value` patches for JSON or TOML config files and prefer plugin verifier specs over shell commands when the plugin catalog is sufficient. Do not invent files, do not use absolute paths, do not invent plugin capabilities beyond the provided catalog, and do not include extra keys. \
When you can infer reliable verification steps from the request and plugin catalog, include `tests_command` and/or `safety_command`; otherwise set them to null."
}

fn plugin_edit_deepseek_system_prompt() -> &'static str {
    "You are a guarded plugin-edit planner for a Rust plugin workspace. Return exactly one JSON object and no markdown. \
The JSON must use this shape: {\"summary\":\"short summary\",\"tests_command\":\"optional verifier command or null\",\"safety_command\":\"optional verifier command or null\",\"operations\":[{\"path\":\"relative/path\",\"kind\":\"replace_exact|create_file|delete_file|json_set|toml_set\",\"expected_old_string\":\"required for replace_exact/create_file\",\"expected_sha256\":\"required for delete_file/json_set/toml_set\",\"new_content\":\"required for replace_exact/create_file\",\"pointer\":\"required for json_set\",\"dotted_key\":\"required for toml_set\",\"value\":\"required for json_set/toml_set\"}]}. \
Only emit operations inside the writable plugin roots. Do not modify runtime crates, root manifests, config, .git, target, artifacts, or generated agent docs under docs/agent/**. \
For existing files, use the provided sha256 values as expected preconditions. For create_file, set expected_old_string to an empty string. \
Do not invent plugin capabilities beyond the provided catalog and do not include extra keys."
}

fn deepseek_patch_tool_system_prompt() -> &'static str {
    "You are a guarded patch planner for a Rust workspace running in DeepSeek reasoner tool mode. \
Inspect files with the provided tools before finalizing a patch plan. \
Prefer batch inspection with read_context_files when several files are relevant. \
If a read tool tells you the same files were already inspected, stop looping on context gathering: decide the change strategy and either submit a plan or inspect only one missing file that would materially change the plan. \
When ready, call submit_patch_plan as the only tool call in that assistant turn. \
Do not invent files, paths, plugin capabilities, or command outputs."
}

fn deepseek_plugin_tool_system_prompt() -> &'static str {
    "You are a guarded plugin-edit planner for a Rust plugin workspace running in DeepSeek reasoner tool mode. \
Inspect files and plugin capabilities with the provided read-only tools before finalizing a plugin edit plan. \
Prefer batch inspection with read_context_files so multi-file edits finish in fewer turns. \
Files under docs/agent/** are generated read-only context and must never be modified. \
If a read tool tells you the same files were already inspected, stop looping on context gathering: decide the change strategy (for example new_child_plugin, inline_core_patch, metadata_only, or docs_only) and either submit a plan or inspect only one missing file that would materially change the plan. \
When ready, call submit_plugin_edit_plan as the only tool call in that assistant turn. \
Do not invent files, paths, plugin capabilities, or command outputs."
}

fn deepseek_patch_tools() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_LIST_CONTEXT_FILES,
                "description": "List all allowed workspace files available to the planner with sha256 preconditions.",
                "parameters": empty_tool_parameters(),
                "strict": true
            }
        },
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_READ_CONTEXT_FILE,
                "description": "Read one allowed workspace file by relative path.",
                "parameters": read_context_file_parameters(),
                "strict": true
            }
        },
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_READ_CONTEXT_FILES,
                "description": "Read several allowed workspace files by relative path in a single tool call.",
                "parameters": read_context_files_parameters(),
                "strict": true
            }
        },
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_INSPECT_PLUGIN_CATALOG,
                "description": "Inspect the discovered plugin capability catalog. Optionally filter by plugin_path.",
                "parameters": inspect_plugin_catalog_parameters(),
                "strict": true
            }
        },
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_SUBMIT_PATCH_PLAN,
                "description": "Submit the final guarded patch plan. Use this only when you are done inspecting context.",
                "parameters": plan_schema(),
                "strict": true
            }
        }
    ])
}

fn deepseek_plugin_tools() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_LIST_CONTEXT_FILES,
                "description": "List all plugin context files available to the planner, including which ones are writable versus read-only generated docs.",
                "parameters": empty_tool_parameters(),
                "strict": true
            }
        },
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_READ_CONTEXT_FILE,
                "description": "Read one plugin context file by relative path.",
                "parameters": read_context_file_parameters(),
                "strict": true
            }
        },
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_READ_CONTEXT_FILES,
                "description": "Read several plugin context files by relative path in a single tool call.",
                "parameters": read_context_files_parameters(),
                "strict": true
            }
        },
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_INSPECT_PLUGIN_CATALOG,
                "description": "Inspect the discovered plugin capability catalog. Optionally filter by plugin_path.",
                "parameters": inspect_plugin_catalog_parameters(),
                "strict": true
            }
        },
        {
            "type": "function",
            "function": {
                "name": DEEPSEEK_TOOL_SUBMIT_PLUGIN_EDIT_PLAN,
                "description": "Submit the final guarded plugin edit plan. Use this only when you are done inspecting context.",
                "parameters": plugin_edit_plan_schema(),
                "strict": true
            }
        }
    ])
}

fn empty_tool_parameters() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {}
    })
}

fn read_context_file_parameters() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": {
                "type": "string"
            }
        },
        "required": ["path"]
    })
}

fn read_context_files_parameters() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "paths": {
                "type": "array",
                "items": {
                    "type": "string"
                },
                "minItems": 1
            }
        },
        "required": ["paths"]
    })
}

fn inspect_plugin_catalog_parameters() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "plugin_path": {
                "type": ["string", "null"]
            }
        }
    })
}

fn plan_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summary": {
                "type": "string"
            },
            "tests_command": {
                "type": ["string", "null"]
            },
            "safety_command": {
                "type": ["string", "null"]
            },
            "patches": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string"
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["text", "json_value", "toml_value"]
                        },
                        "find": {
                            "type": "string",
                            "default": ""
                        },
                        "replace": {
                            "type": "string"
                        },
                        "pointer": {
                            "type": ["string", "null"]
                        },
                        "dotted_key": {
                            "type": ["string", "null"]
                        },
                        "value": {
                            "type": ["object", "array", "string", "number", "integer", "boolean", "null"]
                        }
                    },
                    "required": ["path"]
                }
            }
        },
        "required": ["summary", "patches"]
    })
}

fn plugin_edit_plan_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summary": {
                "type": "string"
            },
            "tests_command": {
                "type": ["string", "null"]
            },
            "safety_command": {
                "type": ["string", "null"]
            },
            "operations": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string"
                        },
                        "kind": {
                            "type": "string",
                            "enum": [
                                "replace_exact",
                                "create_file",
                                "delete_file",
                                "json_set",
                                "toml_set"
                            ]
                        },
                        "expected_old_string": {
                            "type": ["string", "null"]
                        },
                        "expected_sha256": {
                            "type": ["string", "null"]
                        },
                        "new_content": {
                            "type": ["string", "null"]
                        },
                        "pointer": {
                            "type": ["string", "null"]
                        },
                        "dotted_key": {
                            "type": ["string", "null"]
                        },
                        "value": {
                            "type": ["object", "array", "string", "number", "integer", "boolean", "null"]
                        }
                    },
                    "required": ["path", "kind"]
                }
            }
        },
        "required": ["summary", "operations"]
    })
}

fn normalize_optional_command(command: Option<String>) -> Option<String> {
    command.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
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

fn model_uses_deepseek_reasoner(model: &str) -> bool {
    model.trim().eq_ignore_ascii_case("deepseek-reasoner")
}

fn deepseek_completion_model(configured_model: &str) -> &str {
    if model_uses_deepseek_reasoner(configured_model) {
        "deepseek-chat"
    } else {
        configured_model
    }
}

fn should_fallback_deepseek_reasoner(err: &RuntimeError) -> bool {
    matches!(
        err,
        RuntimeError::LlmRequestFailed { .. } | RuntimeError::LlmResponseInvalid { .. }
    )
}

fn should_try_inline_deepseek_completion(context_files: &[ContextFile]) -> bool {
    const MAX_INLINE_CONTEXT_FILES: usize = 32;
    const MAX_INLINE_CONTEXT_CHARS: usize = 120_000;

    context_files.len() <= MAX_INLINE_CONTEXT_FILES
        && context_files
            .iter()
            .map(|file| file.path.len() + file.content.len())
            .sum::<usize>()
            <= MAX_INLINE_CONTEXT_CHARS
}

fn combine_reasoner_fallback_errors(
    operation: &str,
    reasoner_err: RuntimeError,
    fallback_err: RuntimeError,
) -> RuntimeError {
    RuntimeError::LlmRequestFailed {
        message: format!(
            "DeepSeek reasoner {operation} failed: {reasoner_err}; one-shot fallback failed: {fallback_err}"
        ),
    }
}

fn parse_tool_arguments<T: DeserializeOwned>(
    raw_arguments: &str,
    tool_name: &str,
) -> Result<T, RuntimeError> {
    serde_json::from_str(raw_arguments).map_err(|err| RuntimeError::LlmResponseInvalid {
        message: format!("tool {tool_name} arguments were not valid JSON: {err}"),
    })
}

fn find_context_file<'a>(
    context_files: &'a [ContextFile],
    path: &str,
) -> Result<&'a ContextFile, RuntimeError> {
    let normalized = normalize_rel_path(path)?;
    context_files
        .iter()
        .find(|file| file.path == normalized)
        .ok_or_else(|| RuntimeError::LlmResponseInvalid {
            message: format!("tool requested unknown context file: {path}"),
        })
}

fn find_context_files<'a>(
    context_files: &'a [ContextFile],
    paths: &[String],
) -> Result<Vec<&'a ContextFile>, RuntimeError> {
    if paths.is_empty() {
        return Err(RuntimeError::LlmResponseInvalid {
            message: "tool requested zero context files".to_string(),
        });
    }

    paths
        .iter()
        .map(|path| find_context_file(context_files, path))
        .collect()
}

fn select_plugin_planner_context_files(
    instruction: &str,
    context_files: &[ContextFile],
    writable_roots: &BTreeSet<String>,
) -> Vec<ContextFile> {
    let mut selected = context_files
        .iter()
        .filter(|file| {
            path_within_writable_surface(&file.path, writable_roots)
                && instruction.contains(&file.path)
        })
        .cloned()
        .collect::<Vec<_>>();

    if selected.is_empty() {
        let mut all = context_files.to_vec();
        sort_plugin_planner_context_files(&mut all, writable_roots);
        return all;
    }

    let selected_paths = selected
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    for file in context_files {
        if is_plugin_architecture_guidance_path(&file.path, writable_roots)
            && !selected_paths.contains(&file.path)
        {
            selected.push(file.clone());
        }
    }
    let selected_paths = selected
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    for file in context_files {
        if writable_roots
            .iter()
            .any(|root| file.path == format!("{root}/Cargo.toml"))
            && !selected_paths.contains(&file.path)
        {
            selected.push(file.clone());
        }
    }

    sort_plugin_planner_context_files(&mut selected, writable_roots);
    selected
}

fn sort_plugin_planner_context_files(
    files: &mut Vec<ContextFile>,
    writable_roots: &BTreeSet<String>,
) {
    files.sort_by(|left, right| {
        plugin_context_file_priority(&left.path, writable_roots)
            .cmp(&plugin_context_file_priority(&right.path, writable_roots))
            .then_with(|| {
                left.path
                    .matches('/')
                    .count()
                    .cmp(&right.path.matches('/').count())
            })
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn plugin_context_file_priority(path: &str, writable_roots: &BTreeSet<String>) -> u8 {
    if is_plugin_architecture_guidance_path(path, writable_roots) {
        0
    } else if writable_roots
        .iter()
        .any(|root| path == format!("{root}/Cargo.toml"))
    {
        1
    } else if writable_roots
        .iter()
        .any(|root| path.starts_with(&format!("{root}/docs/human/")))
    {
        2
    } else if writable_roots
        .iter()
        .any(|root| path.starts_with(&format!("{root}/src/")))
    {
        3
    } else if writable_roots
        .iter()
        .any(|root| path.starts_with(&format!("{root}/tests/")))
    {
        4
    } else if path_within_read_only_generated_context(path, writable_roots) {
        5
    } else {
        6
    }
}

fn is_plugin_architecture_guidance_path(path: &str, writable_roots: &BTreeSet<String>) -> bool {
    writable_roots
        .iter()
        .any(|root| path == format!("{root}/docs/human/overview.md"))
}

fn render_plugin_catalog_tool_result(
    plugin_catalog: Option<&PromptPluginCatalog>,
    plugin_path: Option<&str>,
) -> Result<Value, RuntimeError> {
    let Some(catalog) = plugin_catalog else {
        return Ok(json!({
            "discovery_root": null,
            "plugins": [],
        }));
    };

    let plugins = match plugin_path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(filter) => catalog
            .plugins
            .iter()
            .filter(|plugin| plugin.plugin_path == filter)
            .cloned()
            .collect::<Vec<_>>(),
        None => catalog.plugins.clone(),
    };

    Ok(json!({
        "discovery_root": catalog.discovery_root,
        "plugins": plugins,
    }))
}

fn build_read_tool_result(
    planner_mode: DeepSeekPlannerMode,
    base_payload: Value,
    observation: &DeepSeekReadObservation,
) -> String {
    let mut payload = match base_payload {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("result".to_string(), other);
            map
        }
    };
    payload.insert("ok".to_string(), Value::Bool(true));
    payload.insert(
        "inspection".to_string(),
        json!({
            "requested_paths": observation.requested_paths,
            "new_paths": observation.new_paths,
            "already_seen_paths": observation.already_seen_paths,
            "consecutive_repeated_reads": observation.consecutive_repeated_reads,
            "requires_change_strategy": observation.requires_change_strategy_hint(),
        }),
    );
    if let Some(hint) = observation.hint(planner_mode) {
        payload.insert("hint".to_string(), Value::String(hint));
    }
    Value::Object(payload).to_string()
}

fn emit_read_observation_diagnostic(
    planner_mode: DeepSeekPlannerMode,
    tool_name: &str,
    observation: &DeepSeekReadObservation,
) {
    if observation.already_seen_paths.is_empty() {
        return;
    }

    emit_llm_diagnostic(
        false,
        format!(
            "reasoner_repeat_read planner_mode={} tool={} requested_paths={:?} already_seen_paths={:?} new_paths={:?} consecutive_repeated_reads={} requires_change_strategy={}",
            match planner_mode {
                DeepSeekPlannerMode::Patch => "patch",
                DeepSeekPlannerMode::Plugin => "plugin",
            },
            tool_name,
            observation.requested_paths,
            observation.already_seen_paths,
            observation.new_paths,
            observation.consecutive_repeated_reads,
            observation.requires_change_strategy_hint(),
        ),
    );
}

fn tool_feedback_error(tool_name: &str, error: &str) -> Value {
    json!({
        "ok": false,
        "tool": tool_name,
        "error": error,
        "hint": "Revise the arguments and call the submit tool again only after the payload satisfies the required shape and writable-path rules."
    })
}

fn truncate_for_error(value: &str, max_chars: usize) -> String {
    let truncated = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn llm_debug_enabled() -> bool {
    env::var(LLM_DEBUG_ENV)
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            !normalized.is_empty() && !matches!(normalized.as_str(), "0" | "false" | "off" | "no")
        })
        .unwrap_or(false)
}

fn emit_llm_diagnostic(force: bool, message: String) {
    if force || llm_debug_enabled() {
        eprintln!("[cordis-runtime][llm] {message}");
    }
}

fn summarize_llm_request(endpoint: &str, request_body: &Value, timeout_ms: u64) -> String {
    let model = request_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let message_count = request_body
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| messages.len())
        .unwrap_or(0);
    let tool_count = request_body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| tools.len())
        .unwrap_or(0);
    let tool_choice = request_body
        .get("tool_choice")
        .map(compact_json_value)
        .unwrap_or_else(|| "-".to_string());
    let response_format = request_body
        .get("response_format")
        .map(compact_json_value)
        .unwrap_or_else(|| "-".to_string());
    let stream = request_body
        .get("stream")
        .and_then(Value::as_bool)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    format!(
        "endpoint={} model={} timeout_ms={} messages={} tools={} tool_choice={} response_format={} stream={}",
        endpoint,
        model,
        timeout_ms,
        message_count,
        tool_count,
        tool_choice,
        response_format,
        stream
    )
}

fn compact_json_value(value: &Value) -> String {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    truncate_for_error(&serialized, 120)
}

fn format_llm_transport_error(err: &reqwest::Error, timeout_ms: u64) -> String {
    if err.is_timeout() {
        format!("request timed out after timeout_ms={timeout_ms}: {err}")
    } else if err.is_connect() {
        format!("connection failed: {err}")
    } else if err.is_body() {
        format!("response body transport failed: {err}")
    } else if err.is_decode() {
        format!("response decode failed: {err}")
    } else if err.is_request() {
        format!("request build/dispatch failed: {err}")
    } else {
        err.to_string()
    }
}

fn format_llm_stream_io_error(err: &std::io::Error, timeout_ms: u64) -> String {
    match err.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            format!("stream read timed out after timeout_ms={timeout_ms}: {err}")
        }
        std::io::ErrorKind::UnexpectedEof => {
            format!("stream ended unexpectedly: {err}")
        }
        _ => err.to_string(),
    }
}

fn merge_stream_field(target: &mut String, delta: &str, append: bool) {
    if delta.is_empty() {
        return;
    }
    if append {
        target.push_str(delta);
        return;
    }
    if target.is_empty() || target == delta || delta.starts_with(target.as_str()) {
        target.clear();
        target.push_str(delta);
        return;
    }
    if !target.starts_with(delta) {
        target.push_str(delta);
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

fn process_deepseek_stream_event(
    payload: &str,
    accumulator: &mut DeepSeekChatMessageAccumulator,
    event_count: &mut usize,
    finish_reason: &mut Option<String>,
    request_summary: &str,
    attempt: usize,
) -> Result<bool, DeepSeekStreamReadError> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }
    if trimmed == "[DONE]" {
        emit_llm_diagnostic(
            false,
            format!(
                "stream_done attempt={} events={} {}",
                attempt, *event_count, request_summary
            ),
        );
        return Ok(true);
    }

    let chunk: DeepSeekChatChunk = serde_json::from_str(trimmed).map_err(|err| {
        DeepSeekStreamReadError::InvalidResponse(format!(
            "invalid streamed DeepSeek chat chunk JSON: {err}; body_preview={}",
            truncate_for_error(trimmed, 800)
        ))
    })?;
    let summary = accumulator
        .apply_chunk(chunk)
        .map_err(|err| DeepSeekStreamReadError::InvalidResponse(err.to_string()))?;
    if let Some(reason) = summary.finish_reason.clone() {
        *finish_reason = Some(reason);
    }
    *event_count += 1;
    emit_llm_diagnostic(
        false,
        format!(
            "stream_event attempt={} event={} delta_reasoning_chars={} delta_content_chars={} delta_tool_calls={} finish_reason={} total_reasoning_chars={} total_content_chars={} {}",
            attempt,
            *event_count,
            summary.delta_reasoning_chars,
            summary.delta_content_chars,
            summary.delta_tool_call_count,
            summary.finish_reason.as_deref().unwrap_or("-"),
            accumulator.reasoning_content.chars().count(),
            accumulator.content.chars().count(),
            request_summary,
        ),
    );
    Ok(false)
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

fn extract_output_text(raw_json: &Value) -> Option<String> {
    if let Some(text) = raw_json.get("output_text").and_then(Value::as_str) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let mut chunks = Vec::new();
    for item in raw_json
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for content in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(text) = content.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    chunks.push(text.trim().to_string());
                }
                continue;
            }

            if let Some(text) = content
                .get("text")
                .and_then(Value::as_object)
                .and_then(|value| value.get("value"))
                .and_then(Value::as_str)
            {
                if !text.trim().is_empty() {
                    chunks.push(text.trim().to_string());
                }
            }
        }
    }

    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n"))
    }
}

fn parse_planner_payload(raw_text: &str) -> Result<PlannerPayload, RuntimeError> {
    serde_json::from_str(raw_text).map_err(|err| RuntimeError::LlmResponseInvalid {
        message: format!("model output was not valid plan JSON: {err}"),
    })
}

fn parse_plugin_planner_payload(raw_text: &str) -> Result<PluginPlannerPayload, RuntimeError> {
    serde_json::from_str(raw_text).map_err(|err| RuntimeError::LlmResponseInvalid {
        message: format!("model output was not valid plugin edit JSON: {err}"),
    })
}

fn validate_patch_paths(
    patches: &[FilePatch],
    allowed_paths: &BTreeSet<String>,
) -> Result<(), RuntimeError> {
    for patch in patches {
        patch.validate_shape()?;
        let normalized = normalize_rel_path(&patch.path)?;
        if !allowed_paths.contains(&normalized) {
            return Err(RuntimeError::LlmResponseInvalid {
                message: format!("model returned disallowed path: {}", patch.path),
            });
        }
    }
    Ok(())
}

fn validate_submitted_patch_payload(
    payload: &PlannerPayload,
    allowed_paths: &BTreeSet<String>,
) -> Result<(), RuntimeError> {
    if payload.patches.is_empty() {
        return Err(RuntimeError::LlmResponseInvalid {
            message: "submit_patch_plan requires at least one patch".to_string(),
        });
    }
    validate_patch_paths(&payload.patches, allowed_paths)
}

fn validate_plugin_operation_paths(
    operations: &[PluginEditOperation],
    writable_roots: &BTreeSet<String>,
) -> Result<(), RuntimeError> {
    for operation in operations {
        let normalized = normalize_plugin_rel_path(&operation.path)?;
        if path_within_read_only_generated_context(&normalized, writable_roots) {
            return Err(RuntimeError::LlmResponseInvalid {
                message: format!(
                    "model returned read-only generated plugin path: {}",
                    operation.path
                ),
            });
        }
        if !path_within_writable_surface(&normalized, writable_roots) {
            return Err(RuntimeError::LlmResponseInvalid {
                message: format!(
                    "model returned disallowed plugin edit path: {}",
                    operation.path
                ),
            });
        }
        validate_plugin_operation_shape(operation, &normalized)?;
    }
    Ok(())
}

fn validate_submitted_plugin_payload(
    payload: &PluginPlannerPayload,
    writable_roots: &BTreeSet<String>,
) -> Result<(), RuntimeError> {
    if payload.operations.is_empty() {
        return Err(RuntimeError::LlmResponseInvalid {
            message: "submit_plugin_edit_plan requires at least one operation".to_string(),
        });
    }
    validate_plugin_operation_paths(&payload.operations, writable_roots)
}

fn validate_plugin_operation_shape(
    operation: &PluginEditOperation,
    normalized_path: &str,
) -> Result<(), RuntimeError> {
    match operation.kind {
        PluginEditOpKind::ReplaceExact => {
            if operation.expected_old_string.is_none() || operation.new_content.is_none() {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!(
                        "replace_exact requires expected_old_string and new_content for {normalized_path}"
                    ),
                });
            }
        }
        PluginEditOpKind::CreateFile => {
            if operation.expected_old_string.is_none() || operation.new_content.is_none() {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!(
                        "create_file requires expected_old_string and new_content for {normalized_path}"
                    ),
                });
            }
        }
        PluginEditOpKind::DeleteFile => {
            if operation.expected_sha256.is_none() {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!("delete_file requires expected_sha256 for {normalized_path}"),
                });
            }
        }
        PluginEditOpKind::JsonSet => {
            if operation.expected_sha256.is_none()
                || operation.pointer.is_none()
                || operation.value.is_none()
            {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!(
                        "json_set requires expected_sha256, pointer, and value for {normalized_path}"
                    ),
                });
            }
        }
        PluginEditOpKind::TomlSet => {
            if operation.expected_sha256.is_none()
                || operation.dotted_key.is_none()
                || operation.value.is_none()
            {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: format!(
                        "toml_set requires expected_sha256, dotted_key, and value for {normalized_path}"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn path_within_writable_surface(path: &str, writable_roots: &BTreeSet<String>) -> bool {
    writable_roots.iter().any(|root| {
        path == format!("{root}/Cargo.toml")
            || path.starts_with(&format!("{root}/src/"))
            || path.starts_with(&format!("{root}/tests/"))
            || path.starts_with(&format!("{root}/docs/human/"))
    })
}

fn path_within_read_only_generated_context(path: &str, writable_roots: &BTreeSet<String>) -> bool {
    writable_roots
        .iter()
        .any(|root| path.starts_with(&format!("{root}/docs/agent/")))
}

fn normalize_rel_path(path: &str) -> Result<String, RuntimeError> {
    let rel_path = Path::new(path);
    if rel_path.is_absolute() {
        return Err(RuntimeError::AutoUpdateInvalidPath {
            path: path.to_string(),
            reason: "absolute path is not allowed".to_string(),
        });
    }
    if rel_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(RuntimeError::AutoUpdateInvalidPath {
            path: path.to_string(),
            reason: "parent directory traversal (..) is not allowed".to_string(),
        });
    }

    let normalized = rel_path
        .components()
        .fold(PathBuf::new(), |mut acc, component| {
            if let Component::Normal(part) = component {
                acc.push(part);
            }
            acc
        });
    Ok(normalized.to_string_lossy().to_string())
}

fn estimate_diff_lines(patches: &[FilePatch]) -> usize {
    patches.iter().map(FilePatch::diff_line_estimate).sum()
}

fn sha256_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

fn api_key_env_looks_like_secret(value: &str) -> bool {
    value.trim_start().starts_with("sk-")
}

#[cfg(test)]
mod tests {
    use super::{
        api_key_env_looks_like_secret, build_plugin_edit_prompt, build_read_tool_result,
        build_user_prompt, discover_plugin_catalog, extract_error_message, extract_output_text,
        normalize_optional_command, normalize_rel_path, path_within_read_only_generated_context,
        path_within_writable_surface, select_plugin_planner_context_files, sha256_text,
        validate_plugin_operation_paths, ContextFile, DeepSeekPlannerMode,
        DeepSeekReadInspectionState, PlanRequest, PluginPlanRequest,
    };
    use crate::kernel::plugin_iteration::{PluginEditOpKind, PluginEditOperation};
    use serde_json::{json, Value};
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn normalize_rel_path_rejects_parent_dir() {
        let err = normalize_rel_path("../escape.txt").expect_err("path should be rejected");
        assert!(err.to_string().contains("parent directory traversal"));
    }

    #[test]
    fn extract_output_text_falls_back_to_output_content() {
        let raw_json = json!({
            "output": [
                {
                    "content": [
                        {
                            "text": "{\"summary\":\"ok\",\"patches\":[]}"
                        }
                    ]
                }
            ]
        });

        let text = extract_output_text(&raw_json).expect("output text should exist");
        assert_eq!(text, "{\"summary\":\"ok\",\"patches\":[]}");
    }

    #[test]
    fn api_key_env_secret_detection_catches_project_keys() {
        assert!(api_key_env_looks_like_secret("sk-proj-demo"));
        assert!(!api_key_env_looks_like_secret("OPENAI_API_KEY"));
    }

    #[test]
    fn extract_error_message_falls_back_to_detail() {
        let message =
            extract_error_message("{\"detail\":\"invalid service token\"}").expect("detail");
        assert_eq!(message, "invalid service token");
    }

    #[test]
    fn build_user_prompt_includes_generic_plugin_verifier_guidance_without_catalog() {
        let request = PlanRequest {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            instruction: "Update the doc".to_string(),
            paths: vec!["README.md".to_string()],
            manual_approved: false,
        };
        let files = vec![ContextFile {
            path: "README.md".to_string(),
            sha256: sha256_text("hello\n"),
            content: "hello\n".to_string(),
        }];

        let prompt = build_user_prompt(&request, &files, None);
        assert!(prompt.contains("plugin verifier spec:"), "prompt: {prompt}");
        assert!(
            prompt.contains("(none discovered for this workspace root)"),
            "prompt: {prompt}"
        );
    }

    #[test]
    fn discover_plugin_catalog_reads_current_fixtures_workspace() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root");
        let catalog = discover_plugin_catalog(&workspace_root).expect("plugin catalog");
        let expr = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.plugin_path == "expr")
            .expect("expr plugin should exist");
        assert_eq!(expr.command_name.as_deref(), Some("Expr"));
        let node = expr
            .nodes
            .iter()
            .find(|node| node.node_id == "expr_entry")
            .expect("expr_entry should exist");
        assert_eq!(node.node_fqn, "expr::expr_entry");
        assert_eq!(
            node.input_schema
                .get("properties")
                .and_then(|value| value.get("expression"))
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str),
            Some("string")
        );
        assert_eq!(
            node.output_schema
                .get("properties")
                .and_then(|value| value.get("value"))
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str),
            Some("number")
        );
    }

    #[test]
    fn build_user_prompt_embeds_discovered_plugin_catalog() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root");
        let catalog = discover_plugin_catalog(&workspace_root).expect("plugin catalog");
        let request = PlanRequest {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            instruction: "Update the doc".to_string(),
            paths: vec!["README.md".to_string()],
            manual_approved: false,
        };
        let files = vec![ContextFile {
            path: "README.md".to_string(),
            sha256: sha256_text("hello\n"),
            content: "hello\n".to_string(),
        }];

        let prompt = build_user_prompt(&request, &files, Some(&catalog));
        assert!(
            prompt.contains("\"plugin_path\": \"expr\""),
            "prompt: {prompt}"
        );
        assert!(
            prompt.contains("\"command_name\": \"Expr\""),
            "prompt: {prompt}"
        );
        assert!(
            prompt.contains("\"node_fqn\": \"expr::expr_entry\""),
            "prompt: {prompt}"
        );
        assert!(prompt.contains("\"expression\""), "prompt: {prompt}");
        assert!(prompt.contains("\"value\""), "prompt: {prompt}");
    }

    #[test]
    fn normalize_optional_command_drops_blank_values() {
        assert_eq!(normalize_optional_command(None), None);
        assert_eq!(normalize_optional_command(Some("   ".to_string())), None);
        assert_eq!(
            normalize_optional_command(Some(" grep -q ok README.md ".to_string())),
            Some("grep -q ok README.md".to_string())
        );
    }

    #[test]
    fn build_plugin_edit_prompt_mentions_writable_roots_and_sha256() {
        let request = PluginPlanRequest {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            instruction: "Update shell docs".to_string(),
            context_paths: vec![
                "plugins/shell/docs/human/overview.md".to_string(),
                "plugins/shell/src/lib.rs".to_string(),
                "plugins/shell/docs/agent/interfaces.json".to_string(),
            ],
            writable_roots: vec!["plugins/shell".to_string()],
            manual_approved: false,
        };
        let files = vec![
            ContextFile {
                path: "plugins/shell/docs/human/overview.md".to_string(),
                sha256: "human789".to_string(),
                content: "# shell\n\nKeep shell behavior isolated.\n".to_string(),
            },
            ContextFile {
                path: "plugins/shell/src/lib.rs".to_string(),
                sha256: "abc123".to_string(),
                content: "pub fn demo() {}\n".to_string(),
            },
            ContextFile {
                path: "plugins/shell/docs/agent/interfaces.json".to_string(),
                sha256: "def456".to_string(),
                content: "{ \"nodes\": [] }\n".to_string(),
            },
        ];

        let prompt = build_plugin_edit_prompt(&request, &files, None);
        assert!(
            prompt.contains("plugins/shell/Cargo.toml"),
            "prompt: {prompt}"
        );
        assert!(prompt.contains("sha256=abc123"), "prompt: {prompt}");
        assert!(
            prompt.contains("plugins/shell/docs/agent/**"),
            "prompt: {prompt}"
        );
        assert!(
            prompt.contains("Read-only context files:\n- plugins/shell/docs/agent/interfaces.json"),
            "prompt: {prompt}"
        );
        assert!(prompt.contains("create_file"), "prompt: {prompt}");
        assert!(
            prompt.contains("Architecture guidance:"),
            "prompt: {prompt}"
        );
        assert!(
            prompt.contains("preferred extension patterns"),
            "prompt: {prompt}"
        );
    }

    #[test]
    fn validate_plugin_operation_paths_rejects_path_outside_writable_surface() {
        let mut writable_roots = BTreeSet::new();
        writable_roots.insert("plugins/shell".to_string());
        let err = validate_plugin_operation_paths(
            &[PluginEditOperation {
                path: "crates/cordis-runtime/src/lib.rs".to_string(),
                kind: PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("pub mod".to_string()),
                expected_sha256: None,
                new_content: Some("pub(crate) mod".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
            &writable_roots,
        )
        .expect_err("path should be rejected");
        assert!(err.to_string().contains("disallowed plugin edit path"));
        let read_only_err = validate_plugin_operation_paths(
            &[PluginEditOperation {
                path: "plugins/shell/docs/agent/interfaces.json".to_string(),
                kind: PluginEditOpKind::JsonSet,
                expected_old_string: None,
                expected_sha256: Some("abc123".to_string()),
                new_content: None,
                pointer: Some("/nodes/0/summary".to_string()),
                dotted_key: None,
                value: Some(json!("updated")),
            }],
            &writable_roots,
        )
        .expect_err("generated agent docs should be rejected");
        assert!(read_only_err
            .to_string()
            .contains("read-only generated plugin path"));
        assert!(path_within_writable_surface(
            "plugins/shell/src/lib.rs",
            &writable_roots
        ));
        assert!(!path_within_writable_surface(
            "plugins/shell/docs/agent/interfaces.json",
            &writable_roots
        ));
        assert!(!path_within_writable_surface(
            "plugins/expr/src/lib.rs",
            &writable_roots
        ));
        assert!(path_within_read_only_generated_context(
            "plugins/shell/docs/agent/interfaces.json",
            &writable_roots
        ));
    }

    #[test]
    fn select_plugin_planner_context_files_prefers_explicit_instruction_paths() {
        let writable_roots = BTreeSet::from(["plugins/expr".to_string()]);
        let files = vec![
            ContextFile {
                path: "plugins/expr/docs/human/overview.md".to_string(),
                sha256: "human".to_string(),
                content: "# expr\n\nUse child plugins.\n".to_string(),
            },
            ContextFile {
                path: "plugins/expr/Cargo.toml".to_string(),
                sha256: "manifest".to_string(),
                content: "[package]\nname = \"expr\"\n".to_string(),
            },
            ContextFile {
                path: "plugins/expr/src/lib.rs".to_string(),
                sha256: "lib".to_string(),
                content: "pub fn evaluate() {}\n".to_string(),
            },
            ContextFile {
                path: "plugins/expr/tests/eval.rs".to_string(),
                sha256: "tests".to_string(),
                content: "#[test]\nfn ok() {}\n".to_string(),
            },
            ContextFile {
                path: "plugins/expr/docs/agent/interfaces.json".to_string(),
                sha256: "agent".to_string(),
                content: "{}\n".to_string(),
            },
        ];

        let selected = select_plugin_planner_context_files(
            "Update plugins/expr/src/lib.rs and plugins/expr/tests/eval.rs for modulo support.",
            &files,
            &writable_roots,
        );
        let selected_paths = selected
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            selected_paths,
            vec![
                "plugins/expr/docs/human/overview.md",
                "plugins/expr/Cargo.toml",
                "plugins/expr/src/lib.rs",
                "plugins/expr/tests/eval.rs",
            ]
        );
    }

    #[test]
    fn select_plugin_planner_context_files_prioritizes_architecture_docs_without_explicit_paths() {
        let writable_roots = BTreeSet::from([
            "plugins/expr".to_string(),
            "plugins/expr/evaluator".to_string(),
        ]);
        let files = vec![
            ContextFile {
                path: "plugins/expr/evaluator/src/core.rs".to_string(),
                sha256: "core".to_string(),
                content: "pub fn eval() {}\n".to_string(),
            },
            ContextFile {
                path: "plugins/expr/evaluator/docs/human/overview.md".to_string(),
                sha256: "evaluator-human".to_string(),
                content: "# expr_evaluator\n\nUse child plugins.\n".to_string(),
            },
            ContextFile {
                path: "plugins/expr/Cargo.toml".to_string(),
                sha256: "manifest".to_string(),
                content: "[package]\nname = \"expr\"\n".to_string(),
            },
            ContextFile {
                path: "plugins/expr/docs/human/overview.md".to_string(),
                sha256: "expr-human".to_string(),
                content: "# expr\n\nNested child plugins.\n".to_string(),
            },
        ];

        let selected = select_plugin_planner_context_files(
            "Add modulo support to expr.",
            &files,
            &writable_roots,
        );
        let selected_paths = selected
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            selected_paths,
            vec![
                "plugins/expr/docs/human/overview.md",
                "plugins/expr/evaluator/docs/human/overview.md",
                "plugins/expr/Cargo.toml",
                "plugins/expr/evaluator/src/core.rs",
            ]
        );
    }

    #[test]
    fn deepseek_read_inspection_tracks_repeated_reads_and_strategy_hint() {
        let mut inspection = DeepSeekReadInspectionState::default();

        let first =
            inspection.record_read(["plugins/expr/src/lib.rs", "plugins/expr/tests/eval.rs"]);
        assert_eq!(first.new_paths.len(), 2);
        assert!(first.already_seen_paths.is_empty());
        assert_eq!(first.consecutive_repeated_reads, 0);
        assert_eq!(first.hint(DeepSeekPlannerMode::Plugin), None);

        let second =
            inspection.record_read(["plugins/expr/tests/eval.rs", "plugins/expr/src/lib.rs"]);
        assert!(second.new_paths.is_empty());
        assert_eq!(
            second.already_seen_paths,
            vec![
                "plugins/expr/src/lib.rs".to_string(),
                "plugins/expr/tests/eval.rs".to_string(),
            ]
        );
        assert_eq!(second.consecutive_repeated_reads, 1);
        let second_hint = second
            .hint(DeepSeekPlannerMode::Plugin)
            .expect("repeat hint");
        assert!(second_hint.contains("already inspected"));
        assert!(!second_hint.contains("new_child_plugin"));

        let third = inspection.record_read(["plugins/expr/src/lib.rs"]);
        assert!(third.new_paths.is_empty());
        assert_eq!(third.consecutive_repeated_reads, 2);
        let third_hint = third
            .hint(DeepSeekPlannerMode::Plugin)
            .expect("strategy hint");
        assert!(third_hint.contains("change strategy"));
        assert!(third_hint.contains("new_child_plugin"));
        assert!(third_hint.contains("submit_plugin_edit_plan"));
    }

    #[test]
    fn build_read_tool_result_includes_inspection_feedback() {
        let mut inspection = DeepSeekReadInspectionState::default();
        inspection.record_read(["demo.txt"]);
        let repeated = inspection.record_read(["demo.txt"]);

        let result = build_read_tool_result(
            DeepSeekPlannerMode::Patch,
            json!({
                "path": "demo.txt",
                "sha256": "abc123",
                "content": "alpha-old-omega\n",
            }),
            &repeated,
        );
        let parsed: Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(parsed.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(
            parsed
                .get("inspection")
                .and_then(|value| value.get("already_seen_paths"))
                .and_then(Value::as_array)
                .map(|items| items.len()),
            Some(1)
        );
        assert!(parsed
            .get("hint")
            .and_then(Value::as_str)
            .is_some_and(|hint| hint.contains("already inspected")));
    }
}
