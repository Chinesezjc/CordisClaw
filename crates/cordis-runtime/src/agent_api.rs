//! The seam between the agent loop and the host.
//!
//! Everything here is interface: the traits a channel/host implements, and the
//! data crossing the boundary. The loop itself lives elsewhere so the kernel can
//! stay a scheduler + loader — nothing in this module decides policy.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::SessionCaller;
use crate::core::error::RuntimeError;

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
    /// 把本轮的渠道元数据注入插件 payload（`__meta`）。
    ///
    /// 默认原样返回：只有真实 host 才有"这一轮是谁在说话"的概念，测试用的
    /// 假 host 不必实现。模型自己塞进 payload 的 `__meta` 会被真实实现覆盖，
    /// 所以插件可以把它当可信身份用于鉴权。
    fn inject_turn_meta(&self, _session_id: &str, payload: Value) -> Value {
        payload
    }
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
    /// Promote a staged (blocked) iteration. The approval *policy* is the
    /// channel plugin's: it reports the capability per session, the kernel only
    /// enforces it.
    fn agent_approve_iteration(
        &self,
        session_id: &str,
        iteration_id: &str,
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
    /// P4: caller role flags for `session_id`, when the host tracks them.
    /// `None` = host cannot tell (no restriction applied).
    fn agent_session_caller(&self, _session_id: &str) -> Option<SessionCaller> {
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
