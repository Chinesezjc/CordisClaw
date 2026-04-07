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
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const PLAN_SCHEMA_NAME: &str = "cordis_auto_update_plan";
const PLUGIN_EDIT_PLAN_SCHEMA_NAME: &str = "cordis_plugin_edit_plan";

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
        let user_prompt = build_user_prompt(&request, &context_files, plugin_catalog.as_ref());
        let provider = self.config.provider.trim().to_ascii_lowercase();
        let (payload, response_id) = match provider.as_str() {
            "openai" => self.plan_with_openai(&user_prompt)?,
            "deepseek" => self.plan_with_deepseek(&user_prompt)?,
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
        let plugin_catalog = discover_plugin_catalog(workspace_root);
        let user_prompt =
            build_plugin_edit_prompt(&request, &context_files, plugin_catalog.as_ref());
        let provider = self.config.provider.trim().to_ascii_lowercase();
        let (payload, response_id) = match provider.as_str() {
            "openai" => self.plan_plugin_edits_with_openai(&user_prompt)?,
            "deepseek" => self.plan_plugin_edits_with_deepseek(&user_prompt)?,
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
    ) -> Result<(PlannerPayload, Option<String>), RuntimeError> {
        let request_body = json!({
            "model": self.config.model,
            "messages": [
                {
                    "role": "system",
                    "content": deepseek_system_prompt(),
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

        let raw_json = self.send_json_request(
            format!(
                "{}/chat/completions",
                self.config.base_url.trim_end_matches('/')
            ),
            request_body,
        )?;
        let response_id = raw_json
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let output_text = extract_chat_completion_text(&raw_json).ok_or_else(|| {
            RuntimeError::LlmResponseInvalid {
                message: "missing choices[0].message.content in chat completion payload"
                    .to_string(),
            }
        })?;
        Ok((parse_planner_payload(&output_text)?, response_id))
    }

    fn plan_plugin_edits_with_deepseek(
        &self,
        user_prompt: &str,
    ) -> Result<(PluginPlannerPayload, Option<String>), RuntimeError> {
        let request_body = json!({
            "model": self.config.model,
            "messages": [
                {
                    "role": "system",
                    "content": plugin_edit_deepseek_system_prompt(),
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

        let raw_json = self.send_json_request(
            format!(
                "{}/chat/completions",
                self.config.base_url.trim_end_matches('/')
            ),
            request_body,
        )?;
        let response_id = raw_json
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let output_text = extract_chat_completion_text(&raw_json).ok_or_else(|| {
            RuntimeError::LlmResponseInvalid {
                message: "missing choices[0].message.content in chat completion payload"
                    .to_string(),
            }
        })?;
        Ok((parse_plugin_planner_payload(&output_text)?, response_id))
    }

    fn send_json_request(
        &self,
        endpoint: String,
        request_body: Value,
    ) -> Result<Value, RuntimeError> {
        let api_key = resolve_api_key(&self.config)?;
        let mut http_request = self
            .client
            .post(endpoint)
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

        let response = http_request.json(&request_body).send().map_err(|err| {
            RuntimeError::LlmRequestFailed {
                message: err.to_string(),
            }
        })?;

        let status = response.status();
        let raw_body = response
            .text()
            .map_err(|err| RuntimeError::LlmRequestFailed {
                message: err.to_string(),
            })?;

        if !status.is_success() {
            let message = extract_error_message(&raw_body)
                .unwrap_or_else(|| format!("status={} body={}", status, raw_body.trim()));
            return Err(RuntimeError::LlmRequestFailed { message });
        }

        serde_json::from_str(&raw_body).map_err(|err| RuntimeError::LlmResponseInvalid {
            message: format!("invalid JSON response: {err}"),
        })
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
        prompt.push_str("/docs/agent/**, ");
        prompt.push_str(root);
        prompt.push_str("/docs/human/**\n");
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
    prompt.push_str("\nGeneric verifier command formats:\n");
    prompt.push_str("- shell command string executed in the workspace root\n");
    prompt.push_str("- plugin verifier spec: plugin:{\"plugin_path\":\"<plugin_path>\",\"node_id\":\"<node_id>\",\"payload_json\":{},\"expect_substring\":\"<expected text>\",\"fixtures_root\":\"<optional fixtures root>\"}\n");
    prompt.push_str("Prefer plugin verifier specs over shell commands when a discovered plugin can validate the requested behavior directly.\n");
    prompt.push_str("\nDiscovered plugin capability catalog:\n");
    match plugin_catalog {
        Some(catalog) => prompt.push_str(&render_plugin_catalog(catalog)),
        None => prompt.push_str("(none discovered for this workspace root)\n"),
    }
    prompt.push_str("\nCurrent file contents with sha256 preconditions:\n");
    for file in files {
        prompt.push_str("\n<<<FILE ");
        prompt.push_str(&file.path);
        prompt.push_str(" sha256=");
        prompt.push_str(&file.sha256);
        prompt.push_str(">>>\n");
        prompt.push_str(&file.content);
        if !file.content.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("<<<END FILE>>>\n");
    }
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

fn openai_system_prompt() -> &'static str {
    "You are a guarded patch planner for a Rust workspace. Return only JSON that matches the provided schema. \
Only edit the allowed relative paths. Each patch must use an exact substring from the current file contents in `find`. \
Keep changes minimal, deterministic, and safe. Prefer `json_value`/`toml_value` patches for JSON or TOML config files and prefer plugin verifier specs over shell commands when the plugin catalog is sufficient. \
Do not invent files, do not use absolute paths, do not invent plugin capabilities beyond the provided catalog, and do not include markdown. \
When you can infer reliable verification steps from the request and plugin catalog, include `tests_command` and/or `safety_command`; otherwise leave them null or omit them."
}

fn plugin_edit_openai_system_prompt() -> &'static str {
    "You are a guarded plugin-edit planner for a Rust plugin workspace. Return only JSON that matches the provided schema. \
Only emit operations inside the writable plugin roots. Do not modify runtime crates, root manifests, config, .git, target, or artifacts. \
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
Only emit operations inside the writable plugin roots. Do not modify runtime crates, root manifests, config, .git, target, or artifacts. \
For existing files, use the provided sha256 values as expected preconditions. For create_file, set expected_old_string to an empty string. \
Do not invent plugin capabilities beyond the provided catalog and do not include extra keys."
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

fn extract_chat_completion_text(raw_json: &Value) -> Option<String> {
    raw_json
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|content| !content.is_empty())
        .map(ToString::to_string)
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

fn validate_plugin_operation_paths(
    operations: &[PluginEditOperation],
    writable_roots: &BTreeSet<String>,
) -> Result<(), RuntimeError> {
    for operation in operations {
        let normalized = normalize_plugin_rel_path(&operation.path)?;
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
            || path.starts_with(&format!("{root}/docs/agent/"))
            || path.starts_with(&format!("{root}/docs/human/"))
    })
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
        api_key_env_looks_like_secret, build_plugin_edit_prompt, build_user_prompt,
        discover_plugin_catalog, extract_chat_completion_text, extract_error_message,
        extract_output_text, normalize_optional_command, normalize_rel_path,
        path_within_writable_surface, sha256_text, validate_plugin_operation_paths, ContextFile,
        PlanRequest, PluginPlanRequest,
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
    fn extract_chat_completion_text_reads_first_choice() {
        let raw_json = json!({
            "choices": [
                {
                    "message": {
                        "content": "{\"summary\":\"ok\",\"patches\":[]}"
                    }
                }
            ]
        });

        let text = extract_chat_completion_text(&raw_json).expect("content should exist");
        assert_eq!(text, "{\"summary\":\"ok\",\"patches\":[]}");
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
            context_paths: vec!["plugins/shell/src/lib.rs".to_string()],
            writable_roots: vec!["plugins/shell".to_string()],
            manual_approved: false,
        };
        let files = vec![ContextFile {
            path: "plugins/shell/src/lib.rs".to_string(),
            sha256: "abc123".to_string(),
            content: "pub fn demo() {}\n".to_string(),
        }];

        let prompt = build_plugin_edit_prompt(&request, &files, None);
        assert!(
            prompt.contains("plugins/shell/Cargo.toml"),
            "prompt: {prompt}"
        );
        assert!(prompt.contains("sha256=abc123"), "prompt: {prompt}");
        assert!(prompt.contains("create_file"), "prompt: {prompt}");
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
        assert!(path_within_writable_surface(
            "plugins/shell/src/lib.rs",
            &writable_roots
        ));
        assert!(!path_within_writable_surface(
            "plugins/expr/src/lib.rs",
            &writable_roots
        ));
    }
}
