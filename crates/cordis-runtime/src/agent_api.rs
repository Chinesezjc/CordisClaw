//! The seam between the agent loop and the host.
//!
//! Everything here is interface: the traits a channel/host implements, and the
//! data crossing the boundary. The loop itself lives elsewhere so the kernel can
//! stay a scheduler + loader — nothing in this module decides policy.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_tools::SessionCaller;
use crate::config::LlmApiConfig;
use crate::core::error::RuntimeError;

pub trait AgentToolHost {
    fn agent_runtime_status(&self) -> Result<Value, RuntimeError>;
    fn agent_list_plugins(&self) -> Result<Value, RuntimeError>;
    fn agent_list_nodes(&self) -> Result<Value, RuntimeError>;
    fn agent_kernel_status(&self) -> Result<Value, RuntimeError>;
    fn agent_kernel_issues(&self) -> Result<Value, RuntimeError>;

    /// What this bot keeps getting wrong, derived from its own transcripts.
    ///
    /// The answer is evidence, not a summary: each signature carries how often
    /// it happens, which sessions hit it, and one example. That is what an
    /// iteration needs to act on a failure instead of guessing at it.
    /// Write a large tool result somewhere the agent can read it back.
    ///
    /// Returns the path, or an error when this host has nowhere to put it —
    /// callers must then keep the content inline rather than drop it.
    fn agent_spill(&self, tool: &str, _text: &str) -> Result<String, RuntimeError> {
        Err(RuntimeError::InvalidArgument {
            message: format!("this host cannot spill {tool} output"),
        })
    }

    /// A short, cacheable summary of the failures that repeat most, for the
    /// system prompt. `None` when there is nothing worth saying.
    ///
    /// This is what couples the report to the model: without it the check only
    /// answers when asked, and a model that never asks learns nothing.
    fn agent_self_check_digest(&self) -> Option<String> {
        None
    }

    fn agent_self_check(&self) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidArgument {
            message: "this host cannot inspect agent transcripts".to_string(),
        })
    }
    /// The structural facts about `session_id`'s neighbourhood.
    ///
    /// Returned shape: `{ me, spawned_by, created, same_room, reachable }`.
    /// Hosts are expected to answer: the loop's doubles return a fixed
    /// neighbour rather than relying on a default, because a default that
    /// silently pretends there is no tree would make the tool lie.
    fn agent_my_team(&self, session_id: &str) -> Result<Value, RuntimeError>;
    /// Deliver a message to a session the caller has been introduced to.
    ///
    /// `caller_session_id` is the sender, `target` an id or address that must
    /// appear in the caller's most recent `agent_my_team` reply.
    fn agent_send_to_session(
        &self,
        caller_session_id: &str,
        target: &str,
        input: &str,
    ) -> Result<Value, RuntimeError>;
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
    /// 把一张**已被请求体预算剥成占位符**的图片取回来（`view_image` 的后端）。
    ///
    /// `path` 是占位符里给出的 `data/media/<sha256>` 相对路径，`sha256` 是
    /// 摘要前缀，二选一。返回 `(相对路径, image_url content part)`：part 由
    /// 调用方挂到下一次请求上，路径原样回给模型。
    fn agent_view_image(
        &self,
        path: Option<&str>,
        sha256: Option<&str>,
    ) -> Result<(String, Value), RuntimeError>;
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

impl AgentToolEvent {
    /// Whether this call actually delivered something.
    ///
    /// A channel tool reports delivery failure inside its payload while the call
    /// itself succeeded — `{"ok":true,"result":{"payload":{"ok":false,…}}}` when
    /// OneBot refuses the connection. Counting that as a sent message reports a
    /// delivery that never happened and puts text in history the room never saw.
    pub fn delivered(&self) -> bool {
        if !self.ok {
            return false;
        }
        // The plugin's verdict sits at `payload.ok`; a wrapped host puts it
        // under `result.payload.ok`. Reading only the wrapped path made every
        // top-level `payload: {ok: false}` — a refused send — count as
        // delivered, so history recorded a message as spoken that never left
        // the process.
        let Some(output) = self.output.as_ref() else {
            return true;
        };
        let payload = output.get("payload").or_else(|| {
            output
                .get("result")
                .and_then(|result| result.get("payload"))
        });
        // No payload verdict: the call's own ok stands.
        payload
            .and_then(|payload| payload.get("ok"))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod delivered_tests {
    use super::*;
    use serde_json::json;

    fn event(output: Value) -> AgentToolEvent {
        AgentToolEvent {
            name: "invoke_plugin".to_string(),
            arguments: json!({}),
            ok: true,
            output: Some(output),
            error: None,
        }
    }

    #[test]
    fn a_refused_send_is_not_delivered() {
        // The shape a plugin node returns: the verdict sits at `payload.ok`.
        let refused = event(json!({
            "plugin_path": "qq",
            "node_id": "qq_send",
            "payload": {"ok": false, "error": "muted until 13:47"}
        }));
        assert!(!refused.delivered());
    }

    #[test]
    fn a_wrapped_refusal_is_not_delivered() {
        let refused = event(json!({"result": {"payload": {"ok": false}}}));
        assert!(!refused.delivered());
    }

    #[test]
    fn a_delivered_send_is_delivered() {
        let sent = event(json!({"payload": {"ok": true, "message": "sent"}}));
        assert!(sent.delivered());
    }

    #[test]
    fn a_call_without_a_verdict_falls_back_to_its_own_ok() {
        assert!(event(json!({"payload": {}})).delivered());
        let failed = AgentToolEvent {
            ok: false,
            ..event(json!({"payload": {"ok": true}}))
        };
        assert!(!failed.delivered());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentTranscriptEntry {
    User {
        content: String,
    },
    Assistant {
        content: String,
        /// Tools this turn actually ran, by node name.
        ///
        /// A channel turn speaks through a tool, so its `content` is empty even
        /// when it answered. Without this the transcript cannot tell "said
        /// nothing" from "said it through qq_send", and every quiet-looking
        /// turn reads as a failure.
        #[serde(default)]
        tools: Vec<String>,
        #[serde(default)]
        response_id: Option<String>,
        /// What the model reasoned before answering, when the provider sent it.
        ///
        /// Kept in the transcript because the live history is pruned: without
        /// this, "why did it say that" is unanswerable the moment the turn
        /// falls out of the window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
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
    /// 执行一次工具调用。
    ///
    /// `attachments` 是本轮的 `view_image` 待附加队列（`image_url` content
    /// part）。队列由**调用方**（回合状态）持有，backend 只负责往里 push：
    /// 把队列收进 `self` 会让它活过回合，而回合状态一旦有跨轮的生命周期，
    /// 下一轮就会读到上一轮留下的东西。不产出图片的 backend 忽略它即可。
    fn execute_tool(
        &mut self,
        name: &str,
        arguments: Value,
        attachments: &mut Vec<Value>,
    ) -> Result<Value, RuntimeError>;
    fn host(&self) -> &Self::Host;
    fn terminal_tool_reply(&self, _name: &str, _output: &Value) -> Option<String> {
        None
    }
    fn tool_scope_label(&self) -> String {
        "agent".to_string()
    }
}

pub trait LlmProvider {
    /// `sink` 为 `Some` 时 provider **可以**逐段推送增量；忽略它也必须正确工作，
    /// 权威结果始终是本函数的返回值。
    fn complete(
        &self,
        body: Value,
        sink: Option<std::sync::Arc<dyn crate::llm_sink::TokenSink>>,
        transport: cordis_plugin_sdk::llm::LlmTransportConfig,
    ) -> Result<LlmCompletionParts, RuntimeError>;
}

#[derive(Debug)]
pub struct LlmCompletionParts {
    pub message: cordis_plugin_sdk::llm::LlmMessage,
    pub response_id: Option<String>,
    pub finish_reason: Option<String>,
}

/// Serializable snapshot of a session, for crash recovery.
///
/// **只存跨轮状态**。一轮之内的临时状态（`no_end_strikes` /
/// `unknown_tool_strikes` / `empty_response_strikes` / `reasoning_only_strikes`
/// 以及 `turn_reasoning`、`image_parts_stripped`、`attached`）不属于快照：
/// 它们随回合开始创建、随回合结束丢弃，写进快照只会把上一轮的计数偷偷带回
/// 下一轮（见 `cordis-agent` 的 `Turn`）。
///
/// 旧快照仍带这些字段也无妨：serde 默认忽略未知字段，所以
/// [`AgentSession`](crate::agent_loop::AgentSessionApi) 的 `from_snapshot`
/// 照常解析、只是不再恢复它们。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionSnapshot {
    pub kind: String,
    pub config: LlmApiConfig,
    pub history: Vec<Value>,
    pub transcript: Vec<AgentTranscriptEntry>,
    pub completed_turns: usize,
    pub estimated_tokens: usize,
    #[serde(default)]
    pub require_end_tool: bool,
    /// `#[serde(default)]` keeps pre-O批 snapshots deserializable.
    #[serde(default)]
    pub soul_key: String,
}
